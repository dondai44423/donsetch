//! The agent's view of an extracted document: what `web_fetch` returns
//! for `toc`, `section`, `focus` and `must_contain`, and the page window
//! `offset`/`max_chars` select.
//!
//! Every view is a pure function of the document markdown, so the same
//! page and the same view always render the same text, and a
//! `next_offset` from one call resumes that rendering in the next. Every
//! extraction path (HTML, PDF, feeds, site adapters, plain text) is
//! viewed the same way, through its markdown headings.
//!
//! Section ids (`s1`, `s2`, ...) number the markdown headings in document
//! order. The outline, `section`, focus groups and probe hits all use the
//! same numbering.

use super::focus;
use super::language::{self, LanguageInfo};
use serde_json::{Value, json};

/// Most terms one `must_contain` call checks.
pub const MUST_CONTAIN_MAX_TERMS: usize = 8;
/// Longest single `must_contain` term, in chars.
pub const TERM_MAX_CHARS: usize = 500;
/// Most passages one focus view returns.
const FOCUS_MAX_PASSAGES: usize = 40;
/// A passage is kept when it scores at least this share of the best one.
const FOCUS_KEEP_RATIO: f64 = 0.3;
/// A passage that states a figure ("EUR 15 million", "7%", "$0.0033 per GB")
/// scores this much more when the query asks how much, how many or how big.
const FIGURE_BOOST: f64 = 1.6;
/// A passage found only through a synonym of a query word counts for this
/// share of a direct hit.
const SYNONYM_WEIGHT: f64 = 0.5;
/// Most sections the "also matched" note names.
const ALSO_MATCHED_SECTIONS: usize = 6;
/// Excerpt context on each side of a probe hit, in chars.
const EXCERPT_PAD: usize = 80;
/// Code fences longer than this are ranked in windows of `CODE_WINDOW` lines.
const CODE_SPLIT_LINES: usize = 60;
const CODE_WINDOW: usize = 40;

/// What the caller asked to see of one document.
#[derive(Clone, Debug, Default)]
pub struct View {
    pub focus: Option<String>,
    pub section: Option<String>,
    pub toc: bool,
    pub must_contain: Vec<Term>,
    pub offset: usize,
    pub max_chars: usize,
}

impl View {
    /// A projection rather than the page itself.
    pub fn is_projection(&self) -> bool {
        self.focus.is_some() || self.section.is_some() || self.toc || !self.must_contain.is_empty()
    }
}

/// One `must_contain` term: a phrase or a `/regex/`.
#[derive(Clone, Debug)]
pub struct Term {
    /// The term as the caller wrote it.
    pub raw: String,
    matcher: regex::Regex,
    /// The phrase's words, lowercased; empty for a regex.
    words: Vec<String>,
}

impl Term {
    /// Parse a term. A phrase matches case-insensitively, with any run of
    /// whitespace or markdown emphasis between its words, curly and straight
    /// quotes alike, and any dash (or a space) for a hyphen. `/re/` with
    /// optional `i`, `m`, `s`, `x` or `u` flags is a case-insensitive regex.
    pub fn parse(raw: &str) -> Result<Term, String> {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Err("must_contain: a term is empty".into());
        }
        if trimmed.chars().count() > TERM_MAX_CHARS {
            return Err(format!(
                "must_contain: a term is longer than {TERM_MAX_CHARS} chars"
            ));
        }
        let (source, words, flags) = match regex_parts(trimmed) {
            Some((body, flags)) => (body.to_string(), Vec::new(), flags),
            None => {
                let words: Vec<String> =
                    trimmed.split_whitespace().map(str::to_lowercase).collect();
                (phrase_pattern(trimmed), words, "")
            }
        };
        let matcher = regex::RegexBuilder::new(&source)
            .case_insensitive(true)
            .multi_line(flags.contains('m'))
            .dot_matches_new_line(flags.contains('s'))
            .ignore_whitespace(flags.contains('x'))
            .size_limit(1 << 24)
            .build()
            .map_err(|e| {
                let first = e.to_string();
                let first = first.lines().last().unwrap_or("invalid").trim().to_string();
                format!("must_contain: {trimmed} is not a valid regex ({first})")
            })?;
        Ok(Term {
            raw: trimmed.to_string(),
            matcher,
            words,
        })
    }

    fn is_regex(&self) -> bool {
        self.words.is_empty()
    }

    fn label(&self) -> String {
        if self.is_regex() {
            self.raw.clone()
        } else {
            serde_json::to_string(&self.raw).unwrap_or_else(|_| self.raw.clone())
        }
    }
}

/// `/body/flags` when `raw` is a regex: slashes around a non-empty body and
/// only regex flags after the closing one, so a path such as `/etc/passwd`
/// stays a phrase.
fn regex_parts(raw: &str) -> Option<(&str, &str)> {
    let inner = raw.strip_prefix('/')?;
    let close = inner.rfind('/')?;
    let (body, flags) = (&inner[..close], &inner[close + 1..]);
    (!body.is_empty() && flags.chars().all(|c| "imsxu".contains(c))).then_some((body, flags))
}

/// Between two phrase words: whitespace or markdown emphasis and quoting.
const WORD_GAP: &str = r"[\s*_~`>]+";

/// A phrase as a pattern: its words in order (see [`Term::parse`]), and each
/// quantity in it (`€15M`, `15%`, `12,000`) by value, in whichever spelling the
/// page uses (`super::amount`).
fn phrase_pattern(phrase: &str) -> String {
    let amounts = super::amount::amounts(phrase);
    if amounts.is_empty() {
        return words_pattern(phrase);
    }
    // Pieces meet across whatever the page puts between words, or nothing
    // (`€15M`'s currency is inside the amount's own pattern).
    let gap = WORD_GAP.replace('+', "*");
    let mut pieces: Vec<String> = Vec::new();
    let mut at = 0;
    for amount in amounts {
        let before = &phrase[at..amount.start];
        if !before.trim().is_empty() {
            pieces.push(words_pattern(before));
        }
        pieces.push(amount.pattern);
        at = amount.end;
    }
    if !phrase[at..].trim().is_empty() {
        pieces.push(words_pattern(&phrase[at..]));
    }
    pieces.join(&gap)
}

fn words_pattern(phrase: &str) -> String {
    phrase
        .split_whitespace()
        .map(|word| {
            let mut out = String::new();
            for c in word.chars() {
                match c {
                    '\'' | '\u{2018}' | '\u{2019}' | '\u{02BC}' => {
                        out.push_str("['\u{2018}\u{2019}\u{02BC}]")
                    }
                    '"' | '\u{201C}' | '\u{201D}' | '\u{201E}' => {
                        out.push_str("[\"\u{201C}\u{201D}\u{201E}]")
                    }
                    '-' | '\u{2010}' | '\u{2011}' | '\u{2012}' | '\u{2013}' | '\u{2014}' => {
                        out.push_str("[-\u{2010}\u{2011}\u{2012}\u{2013}\u{2014}\\s]")
                    }
                    c => out.push_str(&regex::escape(c.encode_utf8(&mut [0; 4]))),
                }
            }
            out
        })
        .collect::<Vec<_>>()
        .join(WORD_GAP)
}

// ── Document model ───────────────────────────────────────────

#[derive(Debug)]
struct Heading {
    level: u8,
    text: String,
    /// Byte offset of the heading line.
    start: usize,
    /// End of the section: the next heading of the same or a higher level.
    end: usize,
    /// Index of the enclosing heading.
    parent: Option<usize>,
}

/// The document markdown with its frontmatter and headings located.
struct Doc<'a> {
    text: &'a str,
    /// Where the body starts: after the source frontmatter (title, byline,
    /// URL and description lines) when the document opens with one.
    body: usize,
    headings: Vec<Heading>,
}

fn is_fence(line: &str) -> bool {
    let t = line.trim_start();
    line.len() - t.len() <= 3 && (t.starts_with("```") || t.starts_with("~~~"))
}

fn heading_of(line: &str) -> Option<(u8, &str)> {
    let hashes = line.bytes().take_while(|&b| b == b'#').count();
    if !(1..=6).contains(&hashes) || !line[hashes..].starts_with(' ') {
        return None;
    }
    let text = line[hashes..].trim().trim_end_matches('#').trim();
    (!text.is_empty()).then_some((hashes as u8, text))
}

/// A heading's words without link markup: `[Title](https://...)` reads
/// as `Title` in the outline, in section paths and for section matching.
fn link_text(title: &str) -> String {
    let mut out = String::with_capacity(title.len());
    let mut rest = title;
    while let Some(open) = rest.find('[') {
        let after = &rest[open + 1..];
        let Some(close) = after.find("](") else {
            break;
        };
        let target = &after[close + 2..];
        let Some(end) = target.find(')') else {
            break;
        };
        out.push_str(&rest[..open]);
        out.push_str(&after[..close]);
        rest = &target[end + 1..];
    }
    out.push_str(rest);
    out
}

/// Lines with their byte offsets (`\n` excluded, `\r` trimmed).
fn lines_from(text: &str, from: usize) -> impl Iterator<Item = (usize, &str)> {
    let mut at = from;
    text[from..].split_inclusive('\n').map(move |raw| {
        let start = at;
        at += raw.len();
        (start, raw.trim_end_matches(['\n', '\r']))
    })
}

impl<'a> Doc<'a> {
    fn parse(text: &'a str) -> Doc<'a> {
        let body = frontmatter_end(text);
        let mut headings: Vec<Heading> = Vec::new();
        let mut fenced = false;
        for (start, line) in lines_from(text, body) {
            if is_fence(line) {
                fenced = !fenced;
                continue;
            }
            if fenced {
                continue;
            }
            if let Some((level, title)) = heading_of(line) {
                headings.push(Heading {
                    level,
                    text: link_text(title),
                    start,
                    end: text.len(),
                    parent: None,
                });
            }
        }
        let mut open: Vec<usize> = Vec::new();
        for i in 0..headings.len() {
            while let Some(&top) = open.last() {
                if headings[top].level >= headings[i].level {
                    headings[top].end = headings[i].start;
                    open.pop();
                } else {
                    break;
                }
            }
            headings[i].parent = open.last().copied();
            open.push(i);
        }
        Doc {
            text,
            body,
            headings,
        }
    }

    /// The section a byte offset falls in: the last heading at or before it.
    fn section_at(&self, at: usize) -> Option<usize> {
        match self.headings.partition_point(|h| h.start <= at) {
            0 => None,
            n => Some(n - 1),
        }
    }

    /// `Install › Linux`: the heading path of one section.
    fn path(&self, idx: usize) -> String {
        let mut parts = vec![self.headings[idx].text.as_str()];
        let mut at = self.headings[idx].parent;
        while let Some(p) = at {
            parts.push(&self.headings[p].text);
            at = self.headings[p].parent;
        }
        parts.reverse();
        parts.join(" › ")
    }
}

/// The source frontmatter ends at the first blank line, when the opening
/// block names the source URL; otherwise there is none.
fn frontmatter_end(text: &str) -> usize {
    let Some(blank) = text.find("\n\n") else {
        return 0;
    };
    let head = &text[..blank];
    if head
        .lines()
        .any(|l| l.starts_with("http://") || l.starts_with("https://"))
        && head.lines().count() <= 8
    {
        blank + 2
    } else {
        0
    }
}

fn size_label(chars: usize) -> String {
    if chars >= 1000 {
        format!("{:.1}k", chars as f64 / 1000.0)
    } else {
        chars.to_string()
    }
}

/// The outline of the headings in `within` (all of them when `None`).
fn outline(doc: &Doc, within: Option<usize>) -> String {
    let ids: Vec<usize> = match within {
        None => (0..doc.headings.len()).collect(),
        Some(root) => (root..doc.headings.len())
            .take_while(|&i| doc.headings[i].start < doc.headings[root].end)
            .collect(),
    };
    if ids.is_empty() {
        let chars = doc.text.len() - doc.body;
        return format!(
            "*(no headings: a flat page of {} chars; read it whole or use focus)*\n",
            size_label(chars)
        );
    }
    let base = ids
        .iter()
        .map(|&i| doc.headings[i].level)
        .min()
        .unwrap_or(1);
    let mut out = String::new();
    for &i in &ids {
        let h = &doc.headings[i];
        let indent = "  ".repeat((h.level - base) as usize);
        out.push_str(&format!(
            "{indent}- [s{}] {} · {}\n",
            i + 1,
            h.text,
            size_label(h.end - h.start)
        ));
    }
    out
}

// ── Section lookup ───────────────────────────────────────────

/// The section `wanted` names: an `sN` id, else the heading text (an exact
/// match first, then the first heading containing it). Returns the section
/// and the other candidates: the other exact matches when there is one,
/// else the other headings that contain the text.
fn find_section(doc: &Doc, wanted: &str) -> Option<(usize, Vec<usize>)> {
    // The label the focus view prints (`§s5`) and the outline's (`[s5]`) both
    // name a section, so both are accepted back.
    let wanted = wanted
        .trim()
        .trim_start_matches('§')
        .trim_matches(['[', ']'])
        .trim();
    let lower = wanted.to_lowercase();
    if let Some(n) = lower
        .strip_prefix('s')
        .filter(|d| !d.is_empty() && d.chars().all(|c| c.is_ascii_digit()))
        .and_then(|d| d.parse::<usize>().ok())
    {
        return (n >= 1 && n <= doc.headings.len()).then(|| (n - 1, Vec::new()));
    }
    let matching: Vec<usize> = doc
        .headings
        .iter()
        .enumerate()
        .filter(|(_, h)| h.text.to_lowercase().contains(&lower))
        .map(|(i, _)| i)
        .collect();
    // "Answer 1" is exact; "Answer 10" containing it is not an ambiguity.
    let exact: Vec<usize> = matching
        .iter()
        .copied()
        .filter(|&i| doc.headings[i].text.to_lowercase() == lower)
        .collect();
    let pool = if exact.is_empty() { matching } else { exact };
    let chosen = *pool.first()?;
    Some((chosen, pool.into_iter().skip(1).collect()))
}

// ── Passages (focus) ─────────────────────────────────────────

struct Passage {
    start: usize,
    text: String,
    section: Option<usize>,
    /// For a table row: the table (its header's offset), the header lines
    /// and the row. Rows of one table that are kept print as one table.
    table: Option<TableRow>,
}

struct TableRow {
    id: usize,
    head: String,
    row: String,
}

fn list_item(line: &str) -> Option<usize> {
    let t = line.trim_start();
    let indent = line.len() - t.len();
    let bullet = t.starts_with("- ") || t.starts_with("* ") || t.starts_with("+ ");
    let digits = t.bytes().take_while(u8::is_ascii_digit).count();
    let numbered = digits > 0 && (t[digits..].starts_with(". ") || t[digits..].starts_with(") "));
    (bullet || numbered).then_some(indent)
}

fn is_table_row(line: &str) -> bool {
    line.trim_start().starts_with('|')
}

fn is_table_rule(line: &str) -> bool {
    let t = line.trim();
    t.starts_with('|') && t.contains('-') && t.chars().all(|c| "|-: ".contains(c))
}

/// Split `doc.text[lo..hi]` into rankable passages: paragraphs, top-level
/// list items, table rows (each under its header) and code (long fences
/// in windows). Headings are context, not passages.
fn passages(doc: &Doc, lo: usize, hi: usize) -> Vec<Passage> {
    let mut out: Vec<Passage> = Vec::new();
    let mut cur: Option<(usize, String)> = None;
    let mut list_indent: Option<usize> = None;
    let mut table_head: Option<String> = None;
    let mut table_id = 0usize;
    let mut fence: Option<(usize, String, Vec<&str>)> = None;

    let flush = |cur: &mut Option<(usize, String)>, out: &mut Vec<Passage>| {
        if let Some((start, text)) = cur.take()
            && !text.trim().is_empty()
        {
            out.push(Passage {
                start,
                text: text.trim_end().to_string(),
                section: doc.section_at(start),
                table: None,
            });
        }
    };

    for (start, line) in lines_from(&doc.text[..hi], lo) {
        if let Some((fstart, open, body)) = fence.as_mut() {
            if is_fence(line) {
                let section = doc.section_at(*fstart);
                if body.len() > CODE_SPLIT_LINES {
                    for (k, window) in body.chunks(CODE_WINDOW).enumerate() {
                        out.push(Passage {
                            start: *fstart + k,
                            text: format!("{open}\n{}\n```", window.join("\n")),
                            section,
                            table: None,
                        });
                    }
                } else {
                    out.push(Passage {
                        start: *fstart,
                        text: format!("{open}\n{}\n```", body.join("\n")),
                        section,
                        table: None,
                    });
                }
                fence = None;
            } else {
                body.push(line);
            }
            continue;
        }
        if is_fence(line) {
            flush(&mut cur, &mut out);
            list_indent = None;
            table_head = None;
            fence = Some((start, line.trim().to_string(), Vec::new()));
            continue;
        }
        if line.trim().is_empty() || heading_of(line).is_some() {
            flush(&mut cur, &mut out);
            list_indent = None;
            table_head = None;
            continue;
        }
        if is_table_row(line) {
            match &mut table_head {
                None => {
                    flush(&mut cur, &mut out);
                    table_head = Some(line.to_string());
                    table_id = start;
                }
                Some(head) if is_table_rule(line) && !head.contains('\n') => {
                    head.push('\n');
                    head.push_str(line);
                }
                Some(head) => {
                    out.push(Passage {
                        start,
                        text: format!("{head}\n{line}"),
                        section: doc.section_at(start),
                        table: Some(TableRow {
                            id: table_id,
                            head: head.clone(),
                            row: line.to_string(),
                        }),
                    });
                }
            }
            continue;
        }
        table_head = None;
        if let Some(indent) = list_item(line)
            && list_indent.is_none_or(|base| indent <= base)
        {
            flush(&mut cur, &mut out);
            list_indent = Some(indent);
            cur = Some((start, line.to_string()));
            continue;
        }
        match cur.as_mut() {
            Some((_, text)) => {
                text.push('\n');
                text.push_str(line);
            }
            None => cur = Some((start, line.to_string())),
        }
    }
    if let Some((fstart, open, body)) = fence {
        // An unclosed fence still carries its code.
        out.push(Passage {
            start: fstart,
            text: format!("{open}\n{}", body.join("\n")),
            section: doc.section_at(fstart),
            table: None,
        });
    }
    flush(&mut cur, &mut out);
    out.sort_by_key(|p| p.start);
    out
}

/// The query's words that appear nowhere in `docs` (stopwords never count).
fn absent_words(query: &str, docs: &[Vec<String>], lang: &LanguageInfo) -> Vec<String> {
    let present: std::collections::HashSet<&str> =
        docs.iter().flatten().map(String::as_str).collect();
    let mut absent = Vec::new();
    for word in query.split_whitespace() {
        let word = word.trim_matches(|c: char| !c.is_alphanumeric());
        let tokens = focus::passage_tokens(word, lang);
        if !tokens.is_empty()
            && tokens.iter().all(|t| !present.contains(t.as_str()))
            && !absent.iter().any(|a: &String| a.eq_ignore_ascii_case(word))
        {
            absent.push(word.to_string());
        }
    }
    absent
}

fn quoted_list(words: &[String]) -> String {
    words
        .iter()
        .map(|w| format!("\"{w}\""))
        .collect::<Vec<_>>()
        .join(", ")
}

struct Focused {
    text: String,
    kept: usize,
    of: usize,
}

/// Query words that ask for a number: how much, how many, what price.
const FIGURE_CUES: &[&str] = &[
    "much",
    "many",
    "amount",
    "amounts",
    "cost",
    "costs",
    "price",
    "prices",
    "pricing",
    "fee",
    "fees",
    "fine",
    "fines",
    "penalty",
    "penalties",
    "rate",
    "rates",
    "limit",
    "limits",
    "maximum",
    "minimum",
    "max",
    "min",
    "percent",
    "percentage",
    "percentages",
    "total",
    "threshold",
    "quota",
    "size",
    "number",
];

/// Words used interchangeably for one thing, so a question in one of them
/// finds the page's own. A passage found only this way scores
/// [`SYNONYM_WEIGHT`] of a direct hit.
const SYNONYM_GROUPS: &[&[&str]] = &[
    &[
        "fine",
        "fines",
        "penalty",
        "penalties",
        "sanction",
        "sanctions",
    ],
    &[
        "price", "prices", "pricing", "cost", "costs", "fee", "fees", "charge", "charges",
    ],
    &[
        "limit",
        "limits",
        "cap",
        "ceiling",
        "maximum",
        "max",
        "quota",
        "threshold",
    ],
    &["minimum", "min", "floor"],
    &["amount", "amounts", "sum", "figure", "figures"],
    &["percent", "percentage", "percentages"],
    &["deadline", "due", "cutoff"],
    &["delete", "remove", "erase"],
    &["buy", "purchase", "order"],
    &["error", "errors", "failure", "failures", "exception"],
    &["begin", "start", "launch"],
    &["end", "finish", "terminate"],
];

fn words_of(query: &str) -> Vec<String> {
    query
        .split(|c: char| !c.is_alphanumeric() && c != '$' && c != '€' && c != '£' && c != '%')
        .filter(|w| !w.is_empty())
        .map(str::to_lowercase)
        .collect()
}

/// Does the query ask for a figure? A cue word, or a number or currency in
/// the query itself.
fn wants_figures(query: &str) -> bool {
    let words = words_of(query);
    words.iter().any(|w| FIGURE_CUES.contains(&w.as_str()))
        || query
            .chars()
            .any(|c| c.is_ascii_digit() || "$€£%".contains(c))
}

/// The query's synonyms that the query does not already say.
fn synonyms_of(query: &str) -> Vec<String> {
    let words = words_of(query);
    let mut out: Vec<String> = Vec::new();
    for group in SYNONYM_GROUPS {
        if words.iter().any(|w| group.contains(&w.as_str())) {
            for alt in *group {
                if !words.iter().any(|w| w == alt) && !out.iter().any(|o| o == alt) {
                    out.push((*alt).to_string());
                }
            }
        }
    }
    out
}

/// Does the passage state a figure: an amount with a currency, a percentage,
/// a number with a unit or a magnitude word?
fn has_figure(text: &str) -> bool {
    static FIGURE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(
            r"(?ix)
            [$€£¥]\s?\d
            | \b(?:usd|eur|gbp|chf|jpy|cad|aud)\s?\d
            | \d\s?(?:%|percent\b|million\b|billion\b|thousand\b|k\b|usd\b|eur\b|gbp\b|€|\$|£)
            | \d{1,3}(?:[,\u{202f}\u{a0}]\d{3})+
            | \d+(?:\.\d+)?\s?(?:ms|gb|mb|kb|tb|km|kg|mph|hz|x)\b",
        )
        .expect("static regex")
    });
    FIGURE.is_match(text)
}

/// Headings every passage of the page sits under (its title, mostly):
/// their words say what the page is about, not where a passage is, so they
/// would make the page's own subject match everywhere.
fn subject_headings(doc: &Doc, all: &[Passage]) -> Vec<bool> {
    let mut covered = vec![0usize; doc.headings.len()];
    for p in all {
        let mut at = p.section;
        while let Some(h) = at {
            covered[h] += 1;
            at = doc.headings[h].parent;
        }
    }
    covered
        .iter()
        .map(|&n| all.len() >= 6 && n * 2 >= all.len())
        .collect()
}

fn render_focus(
    doc: &Doc,
    (lo, hi): (usize, usize),
    scope: Option<usize>,
    query: &str,
    lang: &LanguageInfo,
) -> Focused {
    let all = passages(doc, lo, hi);
    // A passage is scored with its heading path: a section whose heading
    // names the topic ranks all of its passages. The headings that every
    // passage sits under are left out, or the page's subject would match
    // everywhere and decide nothing.
    let subject = subject_headings(doc, &all);
    let docs: Vec<Vec<String>> = all
        .iter()
        .map(|p| {
            let mut tokens = focus::passage_tokens(&p.text, lang);
            let mut at = p.section;
            while let Some(h) = at {
                if !subject[h] {
                    tokens.extend(focus::passage_tokens(&doc.headings[h].text, lang));
                }
                at = doc.headings[h].parent;
            }
            tokens
        })
        .collect();
    let query_tokens = focus::passage_tokens(query, lang);
    let mut scores = focus::bm25_rank(&docs, query_tokens.clone());
    // Synonyms add to the passages they find; they never remove a direct hit.
    let alts = synonyms_of(query);
    if !alts.is_empty() {
        let alt_tokens: Vec<String> = focus::passage_tokens(&alts.join(" "), lang)
            .into_iter()
            .filter(|t| !query_tokens.contains(t))
            .collect();
        if !alt_tokens.is_empty() {
            let via = focus::bm25_rank(&docs, alt_tokens);
            for (score, alt) in scores.iter_mut().zip(via) {
                *score += alt * SYNONYM_WEIGHT;
            }
        }
    }
    if wants_figures(query) {
        for (score, p) in scores.iter_mut().zip(&all) {
            if *score > 0.0 && has_figure(&p.text) {
                *score *= FIGURE_BOOST;
            }
        }
    }
    let absent = absent_words(query, &docs, lang);
    let best = scores.iter().copied().fold(0.0f64, f64::max);
    let absent_note = if absent.is_empty() {
        String::new()
    } else {
        format!("; not on the page: {}", quoted_list(&absent))
    };
    if best <= 0.0 {
        return Focused {
            text: format!(
                "*[focus {query:?}: no passage matches{absent_note}. The outline follows; read a part with section=\"sN\".]*\n\n{}",
                outline(doc, scope)
            ),
            kept: 0,
            of: all.len(),
        };
    }
    let mut ranked: Vec<usize> = (0..all.len())
        .filter(|&i| scores[i] >= best * FOCUS_KEEP_RATIO)
        .collect();
    ranked.sort_by(|&a, &b| scores[b].total_cmp(&scores[a]).then(a.cmp(&b)));
    ranked.truncate(FOCUS_MAX_PASSAGES);

    // A query word the page has must show up in what is kept. When the best
    // passages leave one out, its own best passage is added: a view that
    // silently omits half of what was asked for reads as complete.
    let mut kept_tokens: std::collections::HashSet<&str> = ranked
        .iter()
        .flat_map(|&i| docs[i].iter().map(String::as_str))
        .collect();
    let wanted: Vec<&String> = {
        let mut seen: Vec<&String> = Vec::new();
        for t in &query_tokens {
            if !seen.contains(&t) && docs.iter().any(|d| d.contains(t)) {
                seen.push(t);
            }
        }
        seen
    };
    for token in wanted {
        if kept_tokens.contains(token.as_str()) || ranked.len() >= FOCUS_MAX_PASSAGES {
            continue;
        }
        let top = (0..all.len())
            .filter(|&i| scores[i] > 0.0 && !ranked.contains(&i) && docs[i].contains(token))
            .max_by(|&a, &b| scores[a].total_cmp(&scores[b]).then(b.cmp(&a)));
        if let Some(i) = top {
            kept_tokens.extend(docs[i].iter().map(String::as_str));
            ranked.push(i);
        }
    }

    // A question about an amount is answered by the figures, and a table of
    // them scores low next to the prose that explains it. In a section that
    // is already shown, the table rows and the sentences that state a figure
    // are shown too, when they match at all.
    if wants_figures(query) {
        let shown: std::collections::HashSet<Option<usize>> =
            ranked.iter().map(|&i| all[i].section).collect();
        for i in 0..all.len() {
            if ranked.len() >= FOCUS_MAX_PASSAGES {
                break;
            }
            if scores[i] > 0.0
                && shown.contains(&all[i].section)
                && (all[i].table.is_some() || has_figure(&all[i].text))
                && !ranked.contains(&i)
            {
                ranked.push(i);
            }
        }
    }

    // What matched and was not kept is named, by section: the reader decides
    // whether to go on, instead of trusting a cut it cannot see.
    let kept_set: std::collections::HashSet<usize> = ranked.iter().copied().collect();
    let mut below: Vec<(Option<usize>, usize)> = Vec::new();
    for i in (0..all.len()).filter(|i| scores[*i] > 0.0 && !kept_set.contains(i)) {
        match below.iter_mut().find(|(s, _)| *s == all[i].section) {
            Some((_, n)) => *n += 1,
            None => below.push((all[i].section, 1)),
        }
    }
    let below_total: usize = below.iter().map(|(_, n)| n).sum();
    below.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    let also_note = if below_total == 0 {
        String::new()
    } else {
        let named: Vec<String> = below
            .iter()
            .take(ALSO_MATCHED_SECTIONS)
            .map(|(s, n)| format!("{} ×{n}", place(*s)))
            .collect();
        format!(
            "; {below_total} weaker matches not shown ({}{})",
            named.join(", "),
            if below.len() > ALSO_MATCHED_SECTIONS {
                ", …"
            } else {
                ""
            }
        )
    };

    // Group by section; a group ranks by its best passage and reads in
    // document order inside.
    let mut groups: Vec<(Option<usize>, f64, Vec<usize>)> = Vec::new();
    for &i in &ranked {
        match groups.iter_mut().find(|g| g.0 == all[i].section) {
            Some(group) => {
                group.1 = group.1.max(scores[i]);
                group.2.push(i);
            }
            None => groups.push((all[i].section, scores[i], vec![i])),
        }
    }
    groups.sort_by(|a, b| b.1.total_cmp(&a.1));
    let mut text = format!(
        "*[focus {query:?}: {} of {} passages, best first{absent_note}{also_note}]*\n",
        ranked.len(),
        all.len()
    );
    for (section, _, mut members) in groups {
        members.sort_unstable();
        match section {
            Some(s) => text.push_str(&format!("\n### §s{} · {}\n", s + 1, doc.path(s))),
            None => text.push_str("\n### (top of page)\n"),
        }
        let mut open_table: Option<usize> = None;
        for i in members {
            match &all[i].table {
                // Rows of one table that are kept read as one table.
                Some(t) if open_table == Some(t.id) => {
                    text.push_str(&t.row);
                    text.push('\n');
                }
                Some(t) => {
                    text.push('\n');
                    text.push_str(&t.head);
                    text.push('\n');
                    text.push_str(&t.row);
                    text.push('\n');
                    open_table = Some(t.id);
                }
                None => {
                    open_table = None;
                    text.push('\n');
                    text.push_str(&all[i].text);
                    text.push('\n');
                }
            }
        }
    }
    Focused {
        text,
        kept: ranked.len(),
        of: all.len(),
    }
}

// ── Probe (must_contain) ─────────────────────────────────────

/// The probe's answer for the whole term list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// Every term is on the page.
    Match,
    /// Some terms are on the page, the read was complete.
    Partial,
    /// No term is on the page, the read was complete.
    NoMatch,
    /// A term was not found, but part of the page was not read, so its
    /// absence proves nothing.
    Unverified,
}

impl Verdict {
    pub fn as_str(self) -> &'static str {
        match self {
            Verdict::Match => "match",
            Verdict::Partial => "partial",
            Verdict::NoMatch => "no_match",
            Verdict::Unverified => "unverified",
        }
    }
}

/// One whitespace-collapsed line of context around `start..end`.
fn excerpt(text: &str, start: usize, end: usize) -> String {
    let lo = text[..start]
        .char_indices()
        .rev()
        .nth(EXCERPT_PAD - 1)
        .map_or(0, |(b, _)| b);
    let hi = text[end..]
        .char_indices()
        .nth(EXCERPT_PAD)
        .map_or(text.len(), |(b, _)| end + b);
    let body = text[lo..hi]
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    format!(
        "{}{body}{}",
        if lo > 0 { "…" } else { "" },
        if hi < text.len() { "…" } else { "" }
    )
}

fn place(section: Option<usize>) -> String {
    match section {
        Some(s) => format!("§s{}", s + 1),
        None => "top".into(),
    }
}

struct Probed {
    text: String,
    verdict: Verdict,
    found: usize,
    of: usize,
}

fn render_probe(
    doc: &Doc,
    lo: usize,
    hi: usize,
    terms: &[Term],
    scope: Option<usize>,
    incomplete: Option<&str>,
) -> Probed {
    let hay = &doc.text[lo..hi];
    let per_term = match terms.len() {
        1 => 3,
        2 => 2,
        _ => 1,
    };
    let mut found = 0usize;
    let mut lines = String::new();
    for term in terms {
        let hits: Vec<(usize, usize)> = term
            .matcher
            .find_iter(hay)
            .filter(|m| m.end() > m.start())
            .map(|m| (m.start(), m.end()))
            .take(1000)
            .collect();
        if hits.is_empty() {
            lines.push_str(&format!("- {}: not found", term.label()));
            lines.push_str(&near_miss(doc, lo, hi, term));
            lines.push('\n');
            continue;
        }
        found += 1;
        let mut sections: Vec<Option<usize>> = Vec::new();
        for &(s, _) in &hits {
            let at = doc.section_at(lo + s);
            if !sections.contains(&at) {
                sections.push(at);
            }
        }
        // One excerpt per section first, so a term spread over the page
        // shows where it lives rather than three hits in one paragraph.
        let mut shown: Vec<usize> = Vec::new();
        let mut seen: Vec<Option<usize>> = Vec::new();
        for (k, &(s, _)) in hits.iter().enumerate() {
            let at = doc.section_at(lo + s);
            if !seen.contains(&at) {
                seen.push(at);
                shown.push(k);
            }
            if shown.len() == per_term {
                break;
            }
        }
        for k in 0..hits.len() {
            if shown.len() >= per_term {
                break;
            }
            if !shown.contains(&k) {
                shown.push(k);
            }
        }
        shown.sort_unstable();
        let mut wheres: Vec<String> = sections.iter().take(6).map(|&s| place(s)).collect();
        if sections.len() > 6 {
            wheres.push(format!("+{} more", sections.len() - 6));
        }
        lines.push_str(&format!(
            "+ {} ×{}{} in {}\n",
            term.label(),
            hits.len(),
            if hits.len() == 1000 { "+" } else { "" },
            wheres.join(", ")
        ));
        for k in shown {
            let (s, e) = hits[k];
            lines.push_str(&format!("  {}\n", excerpt(hay, s, e)));
        }
    }
    let of = terms.len();
    let verdict = if found == of {
        Verdict::Match
    } else if incomplete.is_some() {
        Verdict::Unverified
    } else if found == 0 {
        Verdict::NoMatch
    } else {
        Verdict::Partial
    };
    let scope_note = scope
        .map(|s| format!(" in §s{} {}", s + 1, doc.headings[s].text))
        .unwrap_or_default();
    let caveat = match (verdict, incomplete) {
        (Verdict::Unverified, Some(why)) => {
            format!(" (part of the page was not read: {why}; a term not found may be there)")
        }
        _ => String::new(),
    };
    let head = match verdict {
        Verdict::Match => "MATCH",
        Verdict::Partial => "PARTIAL",
        Verdict::NoMatch => "NO MATCH",
        Verdict::Unverified => "UNVERIFIED",
    };
    Probed {
        text: format!("must_contain{scope_note}: {head} {found}/{of}{caveat}\n{lines}"),
        verdict,
        found,
        of,
    }
}

/// Why a missing phrase is missing: its words may all sit in one section
/// without forming the phrase, or some of them are nowhere.
fn near_miss(doc: &Doc, lo: usize, hi: usize, term: &Term) -> String {
    if term.words.len() < 2 {
        return String::new();
    }
    let contains = |a: usize, b: usize, w: &str| doc.text[a..b].to_lowercase().contains(w);
    let mut bounds: Vec<(Option<usize>, usize, usize)> = Vec::new();
    let first = doc.headings.iter().position(|h| h.start >= lo);
    let pre_end = first.map_or(hi, |i| doc.headings[i].start.min(hi));
    if pre_end > lo {
        bounds.push((None, lo, pre_end));
    }
    for (i, h) in doc.headings.iter().enumerate() {
        if h.start >= lo && h.start < hi {
            let own_end = doc.headings.get(i + 1).map_or(hi, |n| n.start.min(hi));
            bounds.push((Some(i), h.start, own_end));
        }
    }
    if let Some(&(at, _, _)) = bounds
        .iter()
        .find(|&&(_, a, b)| term.words.iter().all(|w| contains(a, b, w)))
    {
        return format!("; its words all appear in {}, not as one phrase", place(at));
    }
    let missing: Vec<String> = term
        .words
        .iter()
        .filter(|w| !contains(lo, hi, w))
        .cloned()
        .collect();
    if missing.is_empty() || missing.len() == term.words.len() {
        String::new()
    } else {
        format!("; not on the page: {}", quoted_list(&missing))
    }
}

// ── Rendering ────────────────────────────────────────────────

/// A rendered view, ready for the response.
#[derive(Debug)]
pub struct Rendered {
    pub text: String,
    pub next_offset: Option<usize>,
    /// `read_status` of the response: `content`/`partial` for the page,
    /// else the view (`outline`, `section`, `focus`, `probe`).
    pub status: &'static str,
    /// The whole document is in this response.
    pub complete: bool,
    /// View state for the structured result (merged into it).
    pub state: Value,
    /// The probe verdict, when `must_contain` ran.
    pub verdict: Option<Verdict>,
}

/// Render `view` over the document `markdown`. `incomplete` names why part
/// of the document was not read (a partial listing, a size cap), which
/// turns a probe's missing term into `unverified` rather than absent.
pub fn render(markdown: &str, view: &View, incomplete: Option<&str>) -> Rendered {
    let doc = Doc::parse(markdown);
    let page = |text: &str, status: &'static str, state: Value| {
        let (slice, next, past_end) = window(text, view);
        Rendered {
            text: slice,
            next_offset: next,
            status: if past_end { "end_of_document" } else { status },
            complete: false,
            state: ending(state, view, next),
            verdict: None,
        }
    };

    let mut scope: Option<usize> = None;
    let mut also: Vec<usize> = Vec::new();
    if let Some(wanted) = &view.section {
        match find_section(&doc, wanted) {
            Some((found, others)) => {
                scope = Some(found);
                also = others;
            }
            None => {
                let text = format!(
                    "*[section {wanted:?}: no heading matches. The outline follows; pass an sN id.]*\n\n{}",
                    outline(&doc, None)
                );
                return page(&text, "section", json!({"section": "not_found"}));
            }
        }
    }
    let (lo, hi) = match scope {
        Some(s) => (doc.headings[s].start, doc.headings[s].end),
        None => (0, markdown.len()),
    };
    let also_note = if also.is_empty() {
        String::new()
    } else {
        let ids: Vec<String> = also.iter().take(8).map(|i| format!("s{}", i + 1)).collect();
        format!(
            "*[section {:?} also matches {}]*\n\n",
            view.section.as_deref().unwrap_or(""),
            ids.join(", ")
        )
    };

    if !view.must_contain.is_empty() {
        let probed = render_probe(&doc, lo, hi, &view.must_contain, scope, incomplete);
        return Rendered {
            text: probed.text,
            next_offset: None,
            status: "probe",
            complete: false,
            state: json!({"must_contain": {
                "verdict": probed.verdict.as_str(),
                "found": probed.found,
                "of": probed.of,
            }}),
            verdict: Some(probed.verdict),
        };
    }
    if view.toc {
        let text = format!("{also_note}{}", outline(&doc, scope));
        return page(&text, "outline", json!({"sections": doc.headings.len()}));
    }
    if let Some(query) = view.focus.as_deref().filter(|q| !q.trim().is_empty()) {
        let lang = language::detect_from_text(&markdown[lo..hi]);
        let range = (if scope.is_some() { lo } else { doc.body }, hi);
        let focused = render_focus(&doc, range, scope, query, &lang);
        let text = format!("{also_note}{}", focused.text);
        return page(
            &text,
            "focus",
            json!({"focus": {"passages": focused.kept, "of": focused.of}}),
        );
    }
    if let Some(s) = scope {
        let text = format!("{also_note}{}", &markdown[lo..hi]);
        return page(&text, "section", json!({"section": format!("s{}", s + 1)}));
    }
    let (slice, next, past_end) = window(markdown, view);
    let complete = view.offset == 0 && next.is_none() && incomplete.is_none();
    Rendered {
        status: if past_end {
            "end_of_document"
        } else if complete {
            "content"
        } else {
            "partial"
        },
        text: slice,
        next_offset: next,
        complete,
        state: ending(json!({}), view, next),
        verdict: None,
    }
}

/// The window `view` selects of `text`: the slice, the offset that resumes
/// it, and whether the window starts at or past the end. An offset past the
/// end says so in the text instead of returning nothing under a partial label.
fn window(text: &str, view: &View) -> (String, Option<usize>, bool) {
    if view.offset > 0 && view.offset >= text.len() {
        let note = format!(
            "*[end of document: offset {} is at or past its end ({} chars); nothing follows]*\n",
            view.offset,
            text.len()
        );
        return (note, None, true);
    }
    let (slice, next) = super::paginate_public(text, view.offset, view.max_chars);
    (slice, next, false)
}

/// Marks the response that reaches the document's end after a resume, so a
/// loop over `next_offset` can tell the last chunk from a cut one.
fn ending(mut state: Value, view: &View, next: Option<usize>) -> Value {
    if view.offset > 0 && next.is_none() {
        state["end_of_document"] = json!(true);
    }
    state
}

#[cfg(test)]
mod tests {
    use super::*;

    const DOC: &str = "# Widget API\nhttps://example.com/docs\n> The widget reference.\n\n\
Intro text about widgets and how they work.\n\n\
## Install\n\nRun the installer. It needs Rust 1.80.\n\n\
### Linux\n\nOn Linux, use the tarball. The rate limit is 100 requests per minute.\n\n\
### Windows\n\nOn Windows, use the MSI.\n\n\
## Limits\n\n\
| Plan | Requests |\n|---|---|\n| Free | 100/min |\n| Pro | 10000/min |\n\n\
- Bursts are allowed up to twice the **rate** limit.\n- Exceeding it returns HTTP 429.\n\n\
## FAQ\n\nWhy is my request slow? Check the region.\n\n\
```rust\nfn main() { println!(\"## not a heading\"); }\n```\n";

    fn view() -> View {
        View {
            max_chars: 16_000,
            ..View::default()
        }
    }

    fn terms(raw: &[&str]) -> Vec<Term> {
        raw.iter().map(|t| Term::parse(t).unwrap()).collect()
    }

    #[test]
    fn linked_headings_outline_as_their_words() {
        let doc = Doc::parse(
            "# Front\nhttps://example.com/\n\n## [Floods close the line](https://example.com/a)\n\nText.\n\n\
## Read [the report](L1) today\n\nMore.\n",
        );
        let names: Vec<&str> = doc.headings.iter().map(|h| h.text.as_str()).collect();
        assert_eq!(names, ["Floods close the line", "Read the report today"]);
        assert!(find_section(&doc, "floods close the line").is_some());
        assert_eq!(link_text("[a](x) and [b](y)"), "a and b");
        assert_eq!(link_text("no [link here"), "no [link here");
        assert_eq!(link_text("[x](unclosed"), "[x](unclosed");
    }

    #[test]
    fn headings_skip_the_frontmatter_and_fenced_code() {
        let doc = Doc::parse(DOC);
        let names: Vec<&str> = doc.headings.iter().map(|h| h.text.as_str()).collect();
        assert_eq!(names, ["Install", "Linux", "Windows", "Limits", "FAQ"]);
        // A section runs to the next heading of its level or higher.
        let install = &doc.headings[0];
        assert!(DOC[install.start..install.end].contains("MSI"));
        assert!(!DOC[install.start..install.end].contains("Plan"));
        assert_eq!(doc.path(1), "Install › Linux");
    }

    #[test]
    fn a_document_without_a_source_header_has_no_frontmatter() {
        let doc = Doc::parse("## Only\n\nbody\n");
        assert_eq!(doc.body, 0);
        assert_eq!(doc.headings.len(), 1);
    }

    #[test]
    fn the_outline_numbers_every_heading_with_its_size() {
        let r = render(
            DOC,
            &View {
                toc: true,
                ..view()
            },
            None,
        );
        assert_eq!(r.status, "outline");
        assert!(r.text.contains("- [s1] Install · "), "{}", r.text);
        assert!(r.text.contains("  - [s2] Linux · "), "{}", r.text);
        assert!(r.text.contains("- [s5] FAQ"), "{}", r.text);
        assert!(!r.text.contains("Run the installer"));
        assert_eq!(r.state["sections"], 5);
    }

    #[test]
    fn a_flat_page_outline_says_so() {
        let r = render(
            "# T\nhttps://x.test/\n\njust text\n",
            &View {
                toc: true,
                ..view()
            },
            None,
        );
        assert!(r.text.contains("no headings"), "{}", r.text);
    }

    #[test]
    fn toc_inside_a_section_outlines_only_that_subtree() {
        let r = render(
            DOC,
            &View {
                toc: true,
                section: Some("s1".into()),
                ..view()
            },
            None,
        );
        assert!(r.text.contains("[s3] Windows"));
        assert!(!r.text.contains("Limits"), "{}", r.text);
    }

    #[test]
    fn a_section_by_id_or_name_returns_that_subtree_only() {
        for wanted in ["s1", "install", "INSTALL"] {
            let r = render(
                DOC,
                &View {
                    section: Some(wanted.into()),
                    ..view()
                },
                None,
            );
            assert_eq!(r.status, "section");
            assert!(r.text.starts_with("## Install"), "{wanted}: {}", r.text);
            assert!(r.text.contains("MSI"));
            assert!(!r.text.contains("Plan"));
            assert_eq!(r.state["section"], "s1");
        }
    }

    #[test]
    fn a_section_id_may_be_written_as_the_views_print_it() {
        for wanted in ["s1", "§s1", "§S1", "[s1]", " § s1 "] {
            let r = render(
                DOC,
                &View {
                    section: Some(wanted.into()),
                    ..view()
                },
                None,
            );
            assert_eq!(r.state["section"], "s1", "{wanted:?}: {}", r.text);
        }
    }

    #[test]
    fn an_exact_heading_beats_an_earlier_substring_and_only_true_rivals_are_named() {
        let section = |md: &str, wanted: &str| {
            render(
                md,
                &View {
                    section: Some(wanted.into()),
                    ..view()
                },
                None,
            )
            .text
        };
        let md = "## Pricing tiers\n\nA\n\n## Pricing\n\nB\n";
        let text = section(md, "pricing");
        assert!(text.contains("## Pricing\n\nB"), "{text}");
        assert!(!text.contains("also matches"), "{text}");
        // No exact heading: the first containing one, the rest named.
        let text = section(md, "pric");
        assert!(text.contains("## Pricing tiers\n\nA"), "{text}");
        assert!(text.contains("also matches s2"), "{text}");
        // Duplicate exact headings are a real ambiguity.
        let md = "## Answer 1\n\nA\n\n## Answer 10\n\nB\n\n## Answer 1\n\nC\n";
        let text = section(md, "Answer 1");
        assert!(text.contains("## Answer 1\n\nA"), "{text}");
        assert!(text.contains("also matches s3"), "{text}");
        assert!(!text.contains("s2"), "{text}");
    }

    #[test]
    fn a_missing_section_returns_the_outline_not_the_page() {
        let r = render(
            DOC,
            &View {
                section: Some("Billing".into()),
                ..view()
            },
            None,
        );
        assert_eq!(r.state["section"], "not_found");
        assert!(r.text.contains("no heading matches"));
        assert!(r.text.contains("[s4] Limits"));
        assert!(!r.text.contains("Run the installer"), "{}", r.text);
        // An id past the last heading is a miss too.
        let r = render(
            DOC,
            &View {
                section: Some("s9".into()),
                ..view()
            },
            None,
        );
        assert_eq!(r.state["section"], "not_found");
    }

    #[test]
    fn focus_ranks_the_best_passage_first_with_its_section_path() {
        let r = render(
            DOC,
            &View {
                focus: Some("rate limit".into()),
                ..view()
            },
            None,
        );
        assert_eq!(r.status, "focus");
        assert!(r.text.contains("§s4 · Limits"), "{}", r.text);
        assert!(r.text.contains("Bursts are allowed"), "{}", r.text);
        assert!(!r.text.contains("Windows, use the MSI"), "{}", r.text);
        assert!(r.state["focus"]["passages"].as_u64().unwrap() >= 1);
    }

    #[test]
    fn focus_keeps_a_table_row_under_its_header() {
        let r = render(
            DOC,
            &View {
                focus: Some("pro plan".into()),
                ..view()
            },
            None,
        );
        assert!(
            r.text
                .contains("| Plan | Requests |\n|---|---|\n| Pro | 10000/min |"),
            "{}",
            r.text
        );
        assert!(!r.text.contains("| Free | 100/min |"), "{}", r.text);
    }

    #[test]
    fn a_heading_that_names_the_topic_ranks_its_passages() {
        let r = render(
            DOC,
            &View {
                focus: Some("windows".into()),
                ..view()
            },
            None,
        );
        assert!(r.text.contains("§s3 · Install › Windows"), "{}", r.text);
        assert!(r.text.contains("use the MSI"));
    }

    #[test]
    fn a_focus_miss_returns_the_outline_and_names_absent_words() {
        let r = render(
            DOC,
            &View {
                focus: Some("kubernetes".into()),
                ..view()
            },
            None,
        );
        assert_eq!(r.state["focus"]["passages"], 0);
        assert!(r.text.contains("no passage matches"), "{}", r.text);
        assert!(
            r.text.contains("not on the page: \"kubernetes\""),
            "{}",
            r.text
        );
        assert!(r.text.contains("[s1] Install"));
        assert!(!r.text.contains("Run the installer"), "{}", r.text);
    }

    #[test]
    fn focus_names_the_query_words_the_page_lacks_even_on_a_hit() {
        let r = render(
            DOC,
            &View {
                focus: Some("tarball zebra".into()),
                ..view()
            },
            None,
        );
        assert!(r.text.contains("use the tarball"), "{}", r.text);
        assert!(r.text.contains("not on the page: \"zebra\""), "{}", r.text);
    }

    #[test]
    fn focus_never_falls_back_to_the_whole_page() {
        // Every passage matches: the view is still the ranked passages,
        // never the page with a fallback notice.
        let md = "# T\nhttps://x.test/\n\nalpha one\n\nalpha two\n";
        let r = render(
            md,
            &View {
                focus: Some("alpha".into()),
                ..view()
            },
            None,
        );
        assert!(
            r.text.starts_with("*[focus \"alpha\": 2 of 2 passages"),
            "{}",
            r.text
        );
    }

    #[test]
    fn focus_inside_a_section_ranks_only_that_section() {
        let r = render(
            DOC,
            &View {
                focus: Some("rate limit".into()),
                section: Some("Install".into()),
                ..view()
            },
            None,
        );
        assert!(r.text.contains("§s2 · Install › Linux"), "{}", r.text);
        assert!(!r.text.contains("Bursts"), "{}", r.text);
    }

    #[test]
    fn a_long_focus_view_pages_best_first() {
        let mut md = String::from("# T\nhttps://x.test/\n\n");
        for i in 0..60 {
            md.push_str(&format!(
                "## Part {i}\n\nfiller text number {i} about nothing much at all here\n\n"
            ));
        }
        md.push_str("## Answer\n\nthe answer is the needle needle needle\n\n");
        let r = render(
            &md,
            &View {
                focus: Some("needle".into()),
                max_chars: 300,
                ..View::default()
            },
            None,
        );
        assert!(r.text.contains("the answer is the needle"), "{}", r.text);
    }

    /// A page about penalties, as a law firm writes it: the title is the
    /// topic, the fines sit in a table, and the prose around the table is
    /// longer and says "penalty" more often than the table does.
    const PENALTIES: &str = "# EU AI Act Penalties\nhttps://x.test/penalties\n\n\
## Tiered Penalty System\n\n\
Three tiers apply, by the severity of the violation and the size of the company.\n\n\
| Violation | Maximum Penalty | Turnover |\n|---|---|---|\n\
| Prohibited AI | EUR 35 million | 7% |\n\
| General compliance breach | EUR 15 million | 3% |\n\
| Information accuracy | EUR 7.5 million | 1.5% |\n\n\
Similar to the GDPR, the Act employs tiered penalties, and the penalties are set by the type of system, the seriousness of the violation and the size of the company.\n\n\
Small enterprises benefit: the lower of the two amounts applies to them, which is a different rule from the one the GDPR uses for penalties.\n\n\
## Responsible entities\n\n\
Providers, deployers and importers can all face penalties when they do not meet their obligations under the Act.\n\n\
## Enforcement\n\n\
National authorities enforce the Act and decide on penalties, with the AI Office coordinating how penalties are applied across the Union.\n\n\
## FAQ\n\n\
Do penalties apply outside the EU? Yes, when the system's output is used in the Union.\n\n\
Who collects penalties? The national authority of the member state that imposed them.\n";

    #[test]
    fn focus_on_the_page_topic_still_surfaces_the_figure_table() {
        for query in [
            "penalty amounts",
            "how much is the fine",
            "maximum penalty EUR",
        ] {
            let r = render(
                PENALTIES,
                &View {
                    focus: Some(query.into()),
                    ..view()
                },
                None,
            );
            assert!(r.text.contains("EUR 15 million"), "{query}: {}", r.text);
            assert!(r.text.contains("| 7% |"), "{query}: {}", r.text);
        }
    }

    #[test]
    fn a_figure_table_is_shown_beside_prose_that_repeats_the_topic() {
        // The prose says "penalty" six times in a short paragraph, so a row
        // that says it once scores far under the cut. The question asks for
        // amounts: the rows are the answer.
        let md = "# Guide\nhttps://x.test/g\n\n## Tiers\n\n\
Penalty penalty penalty: the penalty rules, the penalty tiers and the penalty process all matter.\n\n\
| Tier | Maximum Penalty |\n|---|---|\n| One | EUR 35 million |\n| Two | EUR 15 million |\n\n\
## Process\n\nAuthorities open a case.\n\n## Appeals\n\nA company may appeal.\n\n\
## Scope\n\nThe rules cover providers.\n\n## Dates\n\nThe rules apply from August.\n\n\
## Contact\n\nWrite to the office.\n";
        for query in ["penalty amounts", "how much is the penalty"] {
            let r = render(
                md,
                &View {
                    focus: Some(query.into()),
                    ..view()
                },
                None,
            );
            assert!(r.text.contains("EUR 15 million"), "{query}: {}", r.text);
            assert!(r.text.contains("EUR 35 million"), "{query}: {}", r.text);
        }
    }

    #[test]
    fn kept_rows_of_one_table_read_as_one_table() {
        let r = render(
            PENALTIES,
            &View {
                focus: Some("EUR million".into()),
                ..view()
            },
            None,
        );
        assert_eq!(
            r.text.matches("| Violation | Maximum Penalty |").count(),
            1,
            "{}",
            r.text
        );
        assert!(r.text.contains("EUR 35 million"), "{}", r.text);
        assert!(r.text.contains("EUR 7.5 million"), "{}", r.text);
    }

    #[test]
    fn a_query_word_the_page_has_is_never_dropped_from_the_view() {
        // "alpha" fills the best passages; "zebra" is in one weak passage.
        let md = "# T\nhttps://x.test/\n\n## A\n\nalpha alpha alpha alpha alpha\n\n\
alpha alpha alpha alpha\n\n## B\n\nthe zebra crossed the road once\n\n## C\n\nunrelated words only\n\n\
## D\n\nmore unrelated words\n\n## E\n\nstill unrelated\n\n## F\n\nand more\n";
        let r = render(
            md,
            &View {
                focus: Some("alpha zebra".into()),
                ..view()
            },
            None,
        );
        assert!(r.text.contains("zebra crossed"), "{}", r.text);
    }

    #[test]
    fn weaker_matches_that_are_not_shown_are_named_by_section() {
        // Direct hits on "fine", and a tail of passages that say "penalty":
        // a synonym finds them, but weakly, and they fall below the cut.
        let mut md = String::from(
            "# T\nhttps://x.test/\n\n## Core\n\nthe fine fine fine fine is stated here\n\n",
        );
        for i in 0..6 {
            md.push_str(&format!(
                "## Aside {i}\n\nan aside number {i} that mentions a penalty once among many other unrelated words of padding\n\n"
            ));
        }
        for i in 0..14 {
            md.push_str(&format!(
                "## Other {i}\n\nunrelated text number {i} about nothing in particular\n\n"
            ));
        }
        let r = render(
            &md,
            &View {
                focus: Some("fine".into()),
                ..view()
            },
            None,
        );
        assert!(r.text.contains("weaker matches not shown"), "{}", r.text);
        assert!(r.text.contains("§s"), "{}", r.text);
        assert!(!r.text.contains("aside number 3"), "{}", r.text);
    }

    #[test]
    fn an_offset_past_the_end_says_so_instead_of_returning_nothing() {
        let r = render(
            DOC,
            &View {
                offset: DOC.len() + 10,
                max_chars: 16_000,
                ..View::default()
            },
            None,
        );
        assert_eq!(r.status, "end_of_document");
        assert!(r.text.contains("end of document"), "{}", r.text);
        assert!(r.next_offset.is_none());
        assert_eq!(r.state["end_of_document"], true);
    }

    #[test]
    fn the_last_chunk_of_a_resumed_read_is_marked_the_end() {
        let first = render(
            DOC,
            &View {
                max_chars: 200,
                ..View::default()
            },
            None,
        );
        let mut at = first.next_offset.unwrap();
        let mut stitched = first.text.clone();
        loop {
            let next = render(
                DOC,
                &View {
                    offset: at,
                    max_chars: 200,
                    ..View::default()
                },
                None,
            );
            stitched.push_str(&next.text);
            match next.next_offset {
                Some(n) => {
                    assert!(next.state.get("end_of_document").is_none());
                    at = n;
                }
                None => {
                    assert_eq!(next.state["end_of_document"], true);
                    break;
                }
            }
        }
        assert!(stitched.contains("## FAQ"));
    }

    #[test]
    fn the_page_view_pages_with_next_offset_and_reports_completeness() {
        let r = render(
            DOC,
            &View {
                max_chars: 200,
                ..View::default()
            },
            None,
        );
        assert_eq!(r.status, "partial");
        assert!(!r.complete);
        let next = r.next_offset.unwrap();
        let rest = render(
            DOC,
            &View {
                max_chars: 100_000,
                offset: next,
                ..View::default()
            },
            None,
        );
        assert!(rest.text.contains("## FAQ"));
        let whole = render(DOC, &view(), None);
        assert_eq!(whole.status, "content");
        assert!(whole.complete);
        assert_eq!(whole.text, DOC);
        let capped = render(DOC, &view(), Some("listing truncated"));
        assert!(!capped.complete);
    }

    // ── must_contain ──

    #[test]
    fn a_found_phrase_names_its_count_sections_and_excerpts() {
        let r = render(
            DOC,
            &View {
                must_contain: terms(&["rate limit"]),
                ..view()
            },
            None,
        );
        assert_eq!(r.status, "probe");
        assert_eq!(r.verdict, Some(Verdict::Match));
        assert!(
            r.text.starts_with("must_contain: MATCH 1/1\n"),
            "{}",
            r.text
        );
        // "**rate** limit" in the list still matches the phrase.
        assert!(
            r.text.contains("+ \"rate limit\" ×2 in §s2, §s4"),
            "{}",
            r.text
        );
        assert!(
            r.text.contains("The rate limit is 100 requests"),
            "{}",
            r.text
        );
        assert_eq!(r.state["must_contain"]["verdict"], "match");
        assert!(!r.text.contains("Run the installer"));
    }

    #[test]
    fn phrases_match_across_case_quotes_dashes_and_line_breaks() {
        let md = "Don\u{2019}t PANIC \u{2014} the\nanswer is forty\u{2011}two.";
        let r = render(
            md,
            &View {
                must_contain: terms(&["don't panic", "the answer", "forty-two", "panic - the"]),
                ..view()
            },
            None,
        );
        assert_eq!(r.verdict, Some(Verdict::Match), "{}", r.text);
    }

    #[test]
    fn a_hyphenated_term_matches_its_spaced_form_but_not_a_joined_one() {
        let r = render(
            "set the rate limit",
            &View {
                must_contain: terms(&["rate-limit"]),
                ..view()
            },
            None,
        );
        assert_eq!(r.verdict, Some(Verdict::Match));
        let r = render(
            "set the ratelimit",
            &View {
                must_contain: terms(&["rate-limit"]),
                ..view()
            },
            None,
        );
        assert_eq!(r.verdict, Some(Verdict::NoMatch));
    }

    #[test]
    fn several_terms_report_each_and_a_partial_verdict() {
        let r = render(
            DOC,
            &View {
                must_contain: terms(&["HTTP 429", "/\\d+\\/min/", "GraphQL"]),
                ..view()
            },
            None,
        );
        assert_eq!(r.verdict, Some(Verdict::Partial));
        assert!(
            r.text.starts_with("must_contain: PARTIAL 2/3"),
            "{}",
            r.text
        );
        assert!(r.text.contains("+ \"HTTP 429\" ×1 in §s4"), "{}", r.text);
        assert!(r.text.contains("+ /\\d+\\/min/ ×2 in §s4"), "{}", r.text);
        assert!(r.text.contains("- \"GraphQL\": not found\n"), "{}", r.text);
        assert_eq!(r.state["must_contain"]["found"], 2);
        assert_eq!(r.state["must_contain"]["of"], 3);
    }

    #[test]
    fn no_term_found_is_no_match() {
        let r = render(
            DOC,
            &View {
                must_contain: terms(&["GraphQL"]),
                ..view()
            },
            None,
        );
        assert_eq!(r.verdict, Some(Verdict::NoMatch));
        assert!(
            r.text.starts_with("must_contain: NO MATCH 0/1"),
            "{}",
            r.text
        );
    }

    #[test]
    fn a_missing_phrase_whose_words_share_a_section_says_so() {
        let r = render(
            DOC,
            &View {
                must_contain: terms(&["windows tarball"]),
                ..view()
            },
            None,
        );
        assert!(r.text.contains("not found"), "{}", r.text);
        assert!(!r.text.contains("not as one phrase"), "{}", r.text);
        let r = render(
            DOC,
            &View {
                must_contain: terms(&["tarball linux"]),
                ..view()
            },
            None,
        );
        assert!(
            r.text
                .contains("its words all appear in §s2, not as one phrase"),
            "{}",
            r.text
        );
        let r = render(
            DOC,
            &View {
                must_contain: terms(&["tarball zebra"]),
                ..view()
            },
            None,
        );
        assert!(r.text.contains("not on the page: \"zebra\""), "{}", r.text);
    }

    #[test]
    fn an_incomplete_read_never_reports_a_missing_term_as_absent() {
        let r = render(
            DOC,
            &View {
                must_contain: terms(&["GraphQL"]),
                ..view()
            },
            Some("the listing loaded 20 of 80 items"),
        );
        assert_eq!(r.verdict, Some(Verdict::Unverified));
        assert!(r.text.starts_with("must_contain: UNVERIFIED 0/1 (part of the page was not read: the listing loaded 20 of 80 items"), "{}", r.text);
        // A found term is proof whatever was not read.
        let r = render(
            DOC,
            &View {
                must_contain: terms(&["MSI"]),
                ..view()
            },
            Some("cap"),
        );
        assert_eq!(r.verdict, Some(Verdict::Match));
    }

    #[test]
    fn a_probe_scoped_to_a_section_searches_only_there() {
        let r = render(
            DOC,
            &View {
                must_contain: terms(&["MSI"]),
                section: Some("Limits".into()),
                ..view()
            },
            None,
        );
        assert_eq!(r.verdict, Some(Verdict::NoMatch));
        assert!(
            r.text.starts_with("must_contain in §s4 Limits: NO MATCH"),
            "{}",
            r.text
        );
    }

    #[test]
    fn the_probe_searches_the_whole_document_not_a_window() {
        let mut md = "x".repeat(50_000);
        md.push_str(" needle at the end");
        let r = render(
            &md,
            &View {
                must_contain: terms(&["needle"]),
                max_chars: 100,
                ..View::default()
            },
            None,
        );
        assert_eq!(r.verdict, Some(Verdict::Match));
        assert!(r.text.len() < 600, "{}", r.text);
    }

    #[test]
    fn excerpts_prefer_distinct_sections_and_point_at_the_hit() {
        let r = render(
            DOC,
            &View {
                must_contain: terms(&["windows"]),
                ..view()
            },
            None,
        );
        assert!(r.text.contains("in §s3"), "{}", r.text);
        let line = r.text.lines().nth(2).unwrap();
        assert!(line.to_lowercase().contains("windows"), "{line}");
    }

    #[test]
    fn excerpts_land_on_the_hit_in_non_ascii_text() {
        let md = format!("{} İstanbul trip note: the needle is here", "é".repeat(300));
        let r = render(
            &md,
            &View {
                must_contain: terms(&["needle"]),
                ..view()
            },
            None,
        );
        assert!(r.text.contains("the needle is here"), "{}", r.text);
    }

    #[test]
    fn excerpts_survive_case_folding_that_changes_byte_length() {
        // 'İ' (2 bytes) lowercases to "i̇" (3 bytes): offsets must index the
        // real text, never a lowercased copy.
        let prefix = "İSTANBUL İZMİR İÇEL ".repeat(40);
        let md = format!("{prefix}Needle here. {prefix}");
        let r = render(
            &md,
            &View {
                must_contain: terms(&["needle", "/needle\\s+here/"]),
                ..view()
            },
            None,
        );
        assert_eq!(r.verdict, Some(Verdict::Match));
        assert_eq!(r.text.matches("Needle here").count(), 2, "{}", r.text);
    }

    #[test]
    fn regex_terms_and_their_flags_parse_and_paths_stay_phrases() {
        assert!(Term::parse("/CVE-2026-\\d{4}/").unwrap().is_regex());
        assert!(Term::parse("/v\\d+/i").unwrap().is_regex());
        assert!(!Term::parse("/etc/passwd").unwrap().is_regex());
        let r = render(
            "see /etc/passwd for users",
            &View {
                must_contain: terms(&["/etc/passwd"]),
                ..view()
            },
            None,
        );
        assert_eq!(r.verdict, Some(Verdict::Match), "{}", r.text);
    }

    #[test]
    fn invalid_terms_are_refused_with_a_reason() {
        assert!(Term::parse("   ").unwrap_err().contains("empty"));
        assert!(
            Term::parse("/([unclosed/")
                .unwrap_err()
                .contains("not a valid regex")
        );
        let long = "a".repeat(TERM_MAX_CHARS + 1);
        assert!(Term::parse(&long).unwrap_err().contains("longer than"));
        // The longest term still compiles (case folding expands it).
        let longest = "İ".repeat(TERM_MAX_CHARS);
        assert!(Term::parse(&longest).is_ok());
    }

    #[test]
    fn a_regex_that_matches_empty_text_finds_nothing() {
        let r = render(
            "abc",
            &View {
                must_contain: terms(&["/x*/"]),
                ..view()
            },
            None,
        );
        assert_eq!(r.verdict, Some(Verdict::NoMatch), "{}", r.text);
    }

    #[test]
    fn three_or_more_terms_show_one_excerpt_each() {
        let r = render(
            DOC,
            &View {
                must_contain: terms(&["linux", "windows", "rust"]),
                ..view()
            },
            None,
        );
        assert_eq!(
            r.text.lines().filter(|l| l.starts_with("  ")).count(),
            3,
            "{}",
            r.text
        );
    }

    #[test]
    fn the_hit_count_is_capped_and_says_so() {
        let md = "ab ".repeat(2_000);
        let r = render(
            &md,
            &View {
                must_contain: terms(&["ab"]),
                ..view()
            },
            None,
        );
        assert!(r.text.contains("×1000+"), "{}", &r.text[..200]);
    }
}
