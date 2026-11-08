//! Evidence: the sentence that answers the query, not the longest teaser.
//!
//! A result's snippet is shown to an agent that decides from it whether to fetch
//! the page. Two things make a snippet useful for that: it contains the words
//! the query asked about, and it ends where a sentence ends. Every source that
//! returned a URL contributed its own snippet; the one that covers the query best
//! is shown, cut to a sentence boundary, and never padded to a length.
//!
//! Everything here is lexical and native: no model scores a passage.

use std::collections::HashSet;

/// What a snippet may take, in characters. The render clips at the same number,
/// so a picked snippet is shown whole.
pub const EVIDENCE_CHARS: usize = 180;

/// A page passage is read from the page itself, so it may be longer.
pub const PASSAGE_CHARS: usize = 320;

/// Words that carry no topic. Short on purpose: a stop list that grows is a
/// language model written by hand.
const STOP: &[&str] = &[
    "a", "an", "the", "of", "to", "in", "on", "for", "and", "or", "is", "are", "was", "be", "it",
    "its", "this", "that", "with", "as", "at", "by", "from", "how", "what", "why", "when", "which",
    "do", "does", "vs",
];

fn words(s: &str) -> Vec<String> {
    s.split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_lowercase)
        .collect()
}

/// The topic words of a query, without operators or stop words.
fn topic(query: &str) -> Vec<String> {
    let mut seen = HashSet::new();
    words(query)
        .into_iter()
        .filter(|w| w.chars().count() >= 2 && !STOP.contains(&w.as_str()))
        .filter(|w| seen.insert(w.clone()))
        .collect()
}

/// Sentences of `text`: split after `.`, `!`, `?` followed by whitespace, and at
/// line breaks. A dot inside a token (`docs.rs`, `v1.2`, `e.g.`) is not an end.
fn sentences(text: &str) -> Vec<&str> {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut start = 0;
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        let ends = match b {
            b'\n' => true,
            b'.' | b'!' | b'?' => bytes.get(i + 1).is_none_or(|n| n.is_ascii_whitespace()),
            _ => false,
        };
        if ends {
            let s = text[start..=i].trim();
            if !s.is_empty() {
                out.push(s);
            }
            start = i + 1;
        }
        i += 1;
    }
    let tail = text[start..].trim();
    if !tail.is_empty() {
        out.push(tail);
    }
    out
}

/// How much of the topic a passage covers, as a fraction of the topic's words.
fn cover(topic: &[String], passage: &str) -> f64 {
    if topic.is_empty() {
        return 0.0;
    }
    let have: HashSet<String> = words(passage).into_iter().collect();
    topic.iter().filter(|t| have.contains(*t)).count() as f64 / topic.len() as f64
}

/// Cut `text` to at most `max` characters, at a sentence end when one lies in the
/// last 40% of the window, otherwise at a word, with an ellipsis.
pub fn cut(text: &str, max: usize) -> String {
    let text = text.trim();
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= max {
        return text.to_string();
    }
    let floor = max * 3 / 5;
    let window = &chars[..max];
    if let Some(end) = window
        .iter()
        .rposition(|c| matches!(c, '.' | '!' | '?'))
        .filter(|p| *p >= floor)
        .filter(|p| chars.get(p + 1).is_none_or(|n| n.is_whitespace()))
    {
        return window[..=end].iter().collect();
    }
    let space = window
        .iter()
        .rposition(|c| c.is_whitespace())
        .unwrap_or(max);
    let mut head: String = window[..space.max(1)].iter().collect();
    while head.ends_with([',', ';', ':', '-', ' ']) {
        head.pop();
    }
    head.push('…');
    head
}

/// The best window of `text` for the topic: a sentence, or two adjacent ones, no
/// longer than `max`. Ties go to the earlier window (a page states its point
/// first). `None` when the text has no sentence.
fn best_window(topic: &[String], text: &str, max: usize) -> Option<(f64, String)> {
    let sents = sentences(text);
    let mut best: Option<(f64, String)> = None;
    for i in 0..sents.len() {
        for len in 1..=2 {
            let Some(group) = sents.get(i..i + len) else {
                continue;
            };
            let joined = group.join(" ");
            if len == 2 && joined.chars().count() > max {
                continue;
            }
            let c = cover(topic, &joined);
            // A little credit for filling the window: between two equal covers
            // the fuller sentence says more.
            let fill = (joined.chars().count().min(max) as f64) / (max as f64) * 0.05;
            let score = c + fill;
            if best.as_ref().is_none_or(|(s, _)| score > *s + 1e-9) {
                best = Some((score, joined));
            }
        }
    }
    best.map(|(s, w)| (s, cut(&w, max)))
}

/// Share of the query's topic words that `passage` holds: the bench's measure of
/// whether a shown snippet speaks to the question.
pub fn coverage(query: &str, passage: &str) -> f64 {
    cover(&topic(query), passage)
}

/// A date a SERP puts in front of the description ("Mar 10, 2020 · ...",
/// "3 days ago — ..."): it is metadata, and in a 180-character window it costs
/// words that belong to the answer.
fn strip_lead_date(s: &str) -> &str {
    static LEAD: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re = LEAD.get_or_init(|| {
        regex::Regex::new(
            r"^(?:[A-Z][a-z]{2,8}\.? \d{1,2}, \d{4}|\d{1,2} [A-Z][a-z]{2,8}\.? \d{4}|\d+ (?:hours?|days?|weeks?|months?|years?) ago)\s*[·—–-]\s*",
        )
        .expect("lead-date pattern")
    });
    match re.find(s) {
        Some(m) => &s[m.end()..],
        None => s,
    }
}

/// A stub a source returns instead of a description.
fn is_stub(s: &str) -> bool {
    let t = s.trim();
    t.chars().count() < 20 || t.starts_with("Redirecting")
}

/// The snippet to show for a result: across every source's snippet, the window
/// that covers the query's topic best. With no candidate that covers any of it,
/// the first sentence of the most informative one.
pub fn pick(query: &str, candidates: &[String], max: usize) -> String {
    let topic = topic(query);
    let usable: Vec<&String> = candidates.iter().filter(|c| !is_stub(c)).collect();
    let pool: Vec<&String> = if usable.is_empty() {
        candidates.iter().collect()
    } else {
        usable
    };
    let mut best: Option<(f64, String)> = None;
    for c in pool {
        if let Some((score, w)) = best_window(&topic, strip_lead_date(c), max)
            && best.as_ref().is_none_or(|(s, _)| score > *s + 1e-9)
        {
            best = Some((score, w));
        }
    }
    best.map(|(_, w)| w).unwrap_or_default()
}

/// The passage of a fetched page that answers the query: paragraphs are the
/// units, and the best-covering one is cut to `max`. `None` when no paragraph
/// shares a single topic word with the query: a page that does not speak to the
/// query has no passage to offer, and the teaser stays.
pub fn passage(query: &str, markdown: &str, max: usize) -> Option<String> {
    let topic = topic(query);
    if topic.is_empty() {
        return None;
    }
    let mut best: Option<(f64, String)> = None;
    for para in markdown.split("\n\n") {
        let para = para.trim();
        // Headings, rules, tables and code are not prose a person would quote.
        if para.chars().count() < 40
            || para.starts_with(['#', '|', '`', '>', '-', '*', '[', '!'])
            || para.starts_with("---")
        {
            continue;
        }
        let flat = para.split_whitespace().collect::<Vec<_>>().join(" ");
        let Some((score, w)) = best_window(&topic, &flat, max) else {
            continue;
        };
        if cover(&topic, &w) > 0.0 && best.as_ref().is_none_or(|(s, _)| score > *s + 1e-9) {
            best = Some((score, w));
        }
    }
    best.map(|(_, w)| w)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn the_snippet_that_covers_the_query_beats_the_longest_one() {
        let long_off = "Welcome to our site. We are a team of engineers who love building things, \
                        and we write about all kinds of topics every week for you to enjoy.";
        let on_topic = "Tokio's spawn_blocking runs a blocking closure on a dedicated thread pool.";
        let got = pick(
            "tokio spawn_blocking",
            &c(&[long_off, on_topic]),
            EVIDENCE_CHARS,
        );
        assert_eq!(got, on_topic);
    }

    #[test]
    fn a_long_snippet_is_cut_at_a_sentence_not_mid_word() {
        let text = "Rebase replays commits onto a new base. Interactive mode lets you squash \
                    commits, reorder them and edit messages before they are applied, which is the \
                    usual way to tidy a branch before review.";
        let got = pick("git rebase interactive squash", &c(&[text]), 100);
        assert!(got.chars().count() <= 100, "{got}");
        assert!(
            got.ends_with('.') || got.ends_with('…'),
            "ends at a boundary: {got}"
        );
        assert!(!got.contains("revi"), "no mid-word cut: {got}");
    }

    #[test]
    fn dots_inside_tokens_do_not_end_a_sentence() {
        let s = sentences("See docs.rs/tokio for v1.2 details. Then read on.");
        assert_eq!(s, ["See docs.rs/tokio for v1.2 details.", "Then read on."]);
    }

    #[test]
    fn stubs_lose_to_real_text_and_win_only_when_alone() {
        let real = "Docker Compose depends_on controls startup order between services.";
        assert_eq!(
            pick(
                "compose depends_on",
                &c(&["Redirecting...", real]),
                EVIDENCE_CHARS
            ),
            real
        );
        assert_eq!(
            pick(
                "compose",
                &c(&["Redirecting to the page now please"]),
                EVIDENCE_CHARS
            ),
            "Redirecting to the page now please"
        );
        assert_eq!(pick("compose", &[], EVIDENCE_CHARS), "");
    }

    #[test]
    fn a_date_in_front_of_a_description_is_not_part_of_the_snippet() {
        for (raw, want) in [
            (
                "Mar 10, 2020 · Squash your commits using git rebase now.",
                "Squash your commits using git rebase now.",
            ),
            (
                "10 March 2020 — Squash your commits using git rebase now.",
                "Squash your commits using git rebase now.",
            ),
            (
                "3 days ago - Squash your commits using git rebase now.",
                "Squash your commits using git rebase now.",
            ),
            (
                "2020 was the year we squash commits with rebase.",
                "2020 was the year we squash commits with rebase.",
            ),
        ] {
            assert_eq!(
                pick("squash commits rebase", &c(&[raw]), EVIDENCE_CHARS),
                want
            );
        }
    }

    #[test]
    fn a_short_text_is_returned_whole() {
        assert_eq!(cut("short and sweet", 180), "short and sweet");
        let long = "word ".repeat(100);
        let out = cut(&long, 50);
        assert!(out.chars().count() <= 51 && out.ends_with('…'), "{out}");
    }

    #[test]
    fn a_page_passage_is_the_paragraph_that_speaks_to_the_query() {
        let page = "# Title\n\nIntro text about nothing in particular, just filling space here.\n\n\
                    The WAL journal mode lets readers and one writer proceed at once, because \
                    writes go to a separate log file.\n\n- a list item that mentions wal\n\n\
                    Footer links and legal notice text that is long enough to be a paragraph.";
        let got = passage("sqlite wal mode readers", page, PASSAGE_CHARS).unwrap();
        assert!(got.starts_with("The WAL journal mode"), "{got}");
        assert!(passage("kubernetes ingress", page, PASSAGE_CHARS).is_none());
        assert!(
            passage("the of", page, PASSAGE_CHARS).is_none(),
            "no topic, no passage"
        );
    }
}
