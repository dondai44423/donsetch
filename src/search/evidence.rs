//! Evidence: the sentence that answers the query, not the longest teaser.
//!
//! A result's snippet is shown to an agent that decides from it whether to fetch
//! the page. Two things make a snippet useful for that: it contains the words
//! the query asked about, and it ends where a sentence ends. Every source that
//! returned a URL contributed its own snippet; the one that covers the query best
//! is shown, cut to a sentence boundary, and never padded to a length.
//!
//! Everything here is lexical and native: no model scores a passage.

/// What a snippet below the lead results may take, in characters: where a
/// snippet still reliably carries one complete claim. The render clips at the
/// same [`budget`], so a picked snippet is shown whole.
pub const EVIDENCE_CHARS: usize = 180;

/// A page passage is read from the page itself, so it may be longer.
pub const PASSAGE_CHARS: usize = 320;

/// How many results lead the answer and carry the longer budget: the answer is
/// most often among them, and a fuller passage there saves the fetch an agent
/// would otherwise spend to learn what the snippet nearly said.
pub const LEAD_RESULTS: usize = 3;

/// What a snippet may take at `position` (0-based) in the answer: the lead
/// results up to [`PASSAGE_CHARS`], the rest [`EVIDENCE_CHARS`].
pub fn budget(position: usize) -> usize {
    if position < LEAD_RESULTS {
        PASSAGE_CHARS
    } else {
        EVIDENCE_CHARS
    }
}

/// The topic words of a query, without operators or stop words.
fn topic(query: &str) -> Vec<String> {
    super::query::QueryFacts::new(query)
        .terms()
        .map(str::to_string)
        .collect()
}

/// Words a dot follows without ending a sentence.
const ABBREVIATIONS: &[&str] = &[
    "jan", "feb", "mar", "apr", "jun", "jul", "aug", "sep", "sept", "oct", "nov", "dec", "e.g",
    "i.e", "etc", "vs", "cf", "approx", "mr", "mrs", "ms", "dr", "prof", "st", "jr", "sr", "no",
    "fig", "vol", "inc", "ltd", "co", "corp", "al",
];

/// Does the dot at `i` end a sentence? Not when the next word starts lower-case
/// or with a digit ("Oct. 7", "approx. two"), and not after an abbreviation or
/// a single initial ("e.g. Python", "J. Smith"): cutting there dropped the very
/// date a release-date snippet carried.
fn dot_ends(text: &str, i: usize) -> bool {
    let bytes = text.as_bytes();
    if !bytes.get(i + 1).is_none_or(|n| n.is_ascii_whitespace()) {
        return false;
    }
    if let Some(next) = text[i + 1..].trim_start().chars().next()
        && (next.is_lowercase() || next.is_ascii_digit())
    {
        return false;
    }
    let word = text[..i]
        .rsplit(|c: char| c.is_whitespace() || c == '(')
        .next()
        .unwrap_or("")
        .to_lowercase();
    let single_initial = word.chars().count() == 1 && word.chars().all(char::is_alphabetic);
    !(single_initial || ABBREVIATIONS.contains(&word.as_str()))
}

/// Sentences of `text`: split after `.`, `!`, `?` followed by whitespace, and at
/// line breaks, and after `。！？` without requiring spaces. A dot inside a
/// token (`docs.rs`, `v1.2`) or abbreviation (`e.g.`, `Oct.`) is not an end.
fn sentences(text: &str) -> Vec<&str> {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut start = 0;
    for (i, c) in text.char_indices() {
        let end = i + c.len_utf8();
        let ends = match c {
            '\n' | '。' | '！' | '？' => true,
            '.' => dot_ends(text, i),
            '!' | '?' => bytes.get(end).is_none_or(|n| n.is_ascii_whitespace()),
            _ => false,
        };
        if ends {
            let s = text[start..end].trim();
            if !s.is_empty() {
                out.push(s);
            }
            start = end;
        }
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
    topic
        .iter()
        .filter(|term| super::query::matches(term, passage))
        .count() as f64
        / topic.len() as f64
}

/// Cut `text` to at most `max` characters, at a sentence end when one lies in the
/// last 40% of the window, otherwise at a word, with an ellipsis.
pub fn cut(text: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    let text = text.trim();
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= max {
        return text.to_string();
    }
    let floor = max * 3 / 5;
    let window = &chars[..max];
    if let Some(end) = window
        .iter()
        .rposition(|c| matches!(c, '.' | '!' | '?' | '。' | '！' | '？'))
        .filter(|p| *p >= floor)
        .filter(|p| {
            matches!(chars[*p], '。' | '！' | '？')
                || chars.get(p + 1).is_none_or(|n| n.is_whitespace())
        })
    {
        return window[..=end].iter().collect();
    }
    // Reserve the ellipsis too; a word longer than the budget must not make
    // the excerpt exceed it, including for a one-character Unicode window.
    let window = &window[..max - 1];
    let space = window
        .iter()
        .rposition(|c| c.is_whitespace())
        .unwrap_or(window.len());
    let mut head: String = window[..space].iter().collect();
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
            let visible = cut(&joined, max);
            let c = cover(topic, &visible);
            // A little credit for filling the window: between two equal covers
            // the fuller sentence says more.
            let fill = visible.chars().count() as f64 / max as f64 * 0.05;
            let score = c + fill;
            if best.as_ref().is_none_or(|(s, _)| score > *s + 1e-9) {
                best = Some((score, visible));
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

/// Fit an already-selected source excerpt, retaining complete lines when it
/// contains code. Omissions are explicit; commands never become one sentence.
pub(crate) fn quote(text: &str, max: usize) -> String {
    if text.chars().count() <= max || !text.contains('\n') {
        return cut(text, max);
    }
    let mut out = String::new();
    for line in text.lines() {
        let separator = usize::from(!out.is_empty());
        if out.chars().count() + separator + line.chars().count() + 2 > max {
            break;
        }
        if separator > 0 {
            out.push('\n');
        }
        out.push_str(line);
    }
    if out.is_empty() {
        return cut(text.lines().next().unwrap_or(""), max);
    }
    out.push_str("\n…");
    out
}

/// A date a SERP puts in front of the description ("Mar 10, 2020 · ...",
/// "3 days ago — ..."). Keep it attached to whichever passage is selected;
/// removing it made dated search results unusable as evidence of a recent event.
fn split_lead_date(s: &str) -> (&str, &str) {
    static LEAD: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re = LEAD.get_or_init(|| {
        regex::Regex::new(
            r"^(?:[A-Z][a-z]{2,8}\.? \d{1,2}, \d{4}|\d{1,2} [A-Z][a-z]{2,8}\.? \d{4}|\d+ (?:hours?|days?|weeks?|months?|years?) ago)\s*[·—–-]\s*",
        )
        .expect("lead-date pattern")
    });
    match re.find(s) {
        Some(m) => (&s[..m.end()], &s[m.end()..]),
        None => ("", s),
    }
}

/// Retain a source's displayed date when its teaser is replaced from that page.
pub(crate) fn dated_passage(original: &str, passage: &str) -> String {
    let (date, _) = split_lead_date(original);
    if date.is_empty() || passage.starts_with(date) {
        passage.to_string()
    } else {
        format!("{date}{passage}")
    }
}

/// A stub a source returns instead of a description.
pub(crate) fn is_stub(s: &str) -> bool {
    let t = s.trim();
    t.chars().count() < 20 || t.starts_with("Redirecting")
}

/// A description that explicitly promises instructions rather than supplying
/// them. A false positive costs one bounded page read; a false negative leaves
/// the original excerpt. Inline code keeps a usable instruction out of this set.
pub(crate) fn is_teaser(s: &str) -> bool {
    if s.contains('`') {
        return false;
    }
    let lower = s.to_lowercase();
    [
        "learn how to",
        "this guide",
        "this tutorial",
        "this article",
        "step-by-step guide",
        "we will show you",
        "we'll show you",
    ]
    .iter()
    .any(|phrase| lower.contains(phrase))
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
        let (date, body) = split_lead_date(c);
        let room = max.saturating_sub(date.chars().count());
        if room == 0 {
            continue;
        }
        if let Some((score, w)) = best_window(&topic, body, room)
            && best.as_ref().is_none_or(|(s, _)| score > *s + 1e-9)
        {
            best = Some((score, format!("{date}{w}")));
        }
    }
    best.map(|(_, w)| w).unwrap_or_default()
}

/// The passage of a fetched page that speaks to the query. Prose, instructions,
/// table rows and code share the document view's passage boundaries. Structured
/// excerpts retain their source context, columns and indentation.
pub fn passage(query: &str, markdown: &str, max: usize) -> Option<String> {
    if max == 0 {
        return None;
    }
    let topic = topic(query);
    if topic.is_empty() {
        return None;
    }
    let mut best: Option<(f64, String)> = None;
    let howto = super::query::QueryFacts::new(query).shape() == super::query::Shape::HowTo;
    let passages = crate::extract::view::source_passages(markdown);
    for (index, (context, source)) in passages.iter().enumerate() {
        let mut text = source.clone();
        if list_item(source) {
            // Adjacent instructions/ingredients belong together when they fit.
            // Never cross a paragraph or heading to invent a combined recipe.
            for (next_context, next) in passages.iter().skip(index + 1).take(3) {
                if context != next_context
                    || !list_item(next)
                    || context.chars().count().min(80)
                        + text.chars().count()
                        + next.chars().count()
                        + 3
                        > max
                {
                    break;
                }
                text.push('\n');
                text.push_str(next);
            }
        }
        let para = text.trim();
        // Projection notes and image/link-only navigation are not source facts.
        if para.starts_with("*[") || para.starts_with("![") || para.is_empty() {
            continue;
        }
        let structured = para.starts_with(['|', '`', '~']) || list_item(para);
        let candidate = if structured {
            let commands = howto.then(|| command_excerpt(para)).flatten();
            structured_window(&topic, context, commands.as_deref().unwrap_or(para), max)
        } else {
            let flat = para.split_whitespace().collect::<Vec<_>>().join(" ");
            best_window(&topic, &flat, max)
        };
        let Some((mut score, w)) = candidate else {
            continue;
        };
        if howto {
            // In a procedural question, actual source instructions beat a
            // near-equal introduction that merely repeats the question.
            score += if structured { 0.25 } else { 0.0 };
            score -= if is_teaser(&w) { 0.3 } else { 0.0 };
        }
        if cover(&topic, &w) > 0.0 && best.as_ref().is_none_or(|(s, _)| score > *s + 1e-9) {
            best = Some((score, w));
        }
    }
    best.map(|(_, w)| w)
}

fn list_item(text: &str) -> bool {
    let text = text.trim_start();
    let digits = text.bytes().take_while(u8::is_ascii_digit).count();
    text.starts_with("- ")
        || text.starts_with("* ")
        || text.starts_with("+ ")
        || (digits > 0 && (text[digits..].starts_with(". ") || text[digits..].starts_with(") ")))
}

/// Console examples mark input with `$ `. For a procedural question, quote
/// those inputs and their continuations rather than letting download logs win
/// on incidental platform/version words. Omissions remain explicit.
fn command_excerpt(text: &str) -> Option<String> {
    if !text.starts_with("```") && !text.starts_with("~~~") {
        return None;
    }
    if !text.lines().any(|s| s.trim_start().starts_with("$ ")) {
        return None;
    }
    let mut lines = Vec::new();
    let mut continuation = false;
    let mut omitted = false;
    for line in text.lines().skip(1) {
        if line.starts_with("```") || line.starts_with("~~~") {
            break;
        }
        if continuation || line.trim_start().starts_with("$ ") {
            if omitted && !lines.is_empty() {
                lines.push("…");
            }
            lines.push(line);
            continuation = line.trim_end().ends_with('\\');
            omitted = false;
        } else if !line.trim().is_empty() {
            omitted = true;
        }
    }
    (!lines.is_empty()).then(|| lines.join("\n"))
}

/// Quote complete source lines, never join shell commands or YAML indentation
/// into prose. A table's column header is mandatory; oversized headers are left
/// to the fetch tool rather than returning an unlabeled number.
fn structured_window(
    topic: &[String],
    context: &str,
    text: &str,
    max: usize,
) -> Option<(f64, String)> {
    let lines: Vec<&str> = text.lines().collect();
    let fenced = lines
        .first()
        .is_some_and(|s| s.starts_with("```") || s.starts_with("~~~"));
    let body = if fenced {
        let end = lines.len()
            - usize::from(
                lines
                    .last()
                    .is_some_and(|s| s.starts_with("```") || s.starts_with("~~~"))
                    && lines.len() > 1,
            );
        &lines[1..end]
    } else {
        &lines[..]
    };
    let context = cut(context, max.min(80));
    let prefix = if context.is_empty() {
        String::new()
    } else {
        format!("{context}:\n")
    };
    let table = text.starts_with('|');
    let list = list_item(text);
    let mut best: Option<(f64, String)> = None;
    for start in 0..body.len() {
        // Table passages already contain the header and one row.
        if table && start > 0 {
            break;
        }
        let mut quote = prefix.clone();
        for line in &body[start..] {
            let size = quote.chars().count()
                + line.chars().count()
                + usize::from(!quote.is_empty() && !quote.ends_with('\n'));
            if size > max {
                // A long instruction is prose; quote its opening with an
                // ellipsis. Code and table cells must keep their syntax.
                let separator = usize::from(!quote.is_empty() && !quote.ends_with('\n'));
                let room = max.saturating_sub(quote.chars().count() + separator);
                if list && room >= 40 {
                    if separator > 0 {
                        quote.push('\n');
                    }
                    quote.push_str(&cut(line, room));
                }
                break;
            }
            if !quote.is_empty() && !quote.ends_with('\n') {
                quote.push('\n');
            }
            quote.push_str(line);
        }
        let content = quote.strip_prefix(&prefix).unwrap_or(&quote);
        if content.trim().is_empty() || (table && content.lines().count() < body.len()) {
            continue;
        }
        let score = cover(topic, &quote)
            + 0.06
            + content.chars().count().min(max) as f64 / max as f64 * 0.05;
        if cover(topic, &quote) > 0.0 && best.as_ref().is_none_or(|(s, _)| score > *s) {
            best = Some((score, quote));
        }
    }
    best
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
        let hidden = format!(
            "{} Tokio spawn creates asynchronous tasks.",
            "Background without the answer ".repeat(30)
        );
        let got = pick("tokio spawn", &c(&[&hidden, on_topic]), 100);
        assert!(
            got.contains("Tokio") || got.contains("Tokio's"),
            "only shown evidence may earn coverage: {got}"
        );
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
    fn an_abbreviation_or_a_date_does_not_end_a_sentence() {
        let s = sentences("Release date: Oct. 7, 2025. Next one follows.");
        assert_eq!(s, vec!["Release date: Oct. 7, 2025.", "Next one follows."]);
        let s = sentences("Use a runtime, e.g. Tokio, for this. Dr. Smith agrees. J. Doe too.");
        assert_eq!(
            s,
            vec![
                "Use a runtime, e.g. Tokio, for this.",
                "Dr. Smith agrees.",
                "J. Doe too."
            ]
        );
        // A real end before a capitalised word still splits.
        assert_eq!(sentences("It works. Then it stops.").len(), 2);
    }

    #[test]
    fn dots_inside_tokens_do_not_end_a_sentence() {
        let s = sentences("See docs.rs/tokio for v1.2 details. Then read on.");
        assert_eq!(s, ["See docs.rs/tokio for v1.2 details.", "Then read on."]);
        assert_eq!(
            sentences("首都です。人口は多い！本当？はい。"),
            ["首都です。", "人口は多い！", "本当？", "はい。"]
        );
        let long = format!(
            "{}。人口は2025年に約1400万人と推計されます。詳細はこちら。",
            "背景情報".repeat(70)
        );
        let got = pick("人口 2025", &c(&[&long]), 70);
        assert!(got.contains("1400"), "{got}");
        assert_eq!(cut("甲乙。丙丁。戊己。", 7), "甲乙。丙丁。");
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
        assert!(is_teaser(
            "Learn how to configure a reverse proxy in this guide."
        ));
        assert!(!is_teaser(
            "This guide uses `proxy_pass http://backend/;` to replace the prefix."
        ));
        assert!(!is_teaser(
            "Compose waits for the dependency health check to pass."
        ));
    }

    #[test]
    fn a_sources_date_stays_attached_to_its_selected_evidence() {
        for raw in [
            "Mar 10, 2020 · Squash your commits using git rebase now.",
            "10 March 2020 — Squash your commits using git rebase now.",
            "3 days ago - Squash your commits using git rebase now.",
            "2020 was the year we squash commits with rebase.",
        ] {
            assert_eq!(
                pick("squash commits rebase", &c(&[raw]), EVIDENCE_CHARS),
                raw
            );
        }
        assert_eq!(
            dated_passage(
                "3 days ago - Learn how to configure it.",
                "Set Restart=on-failure."
            ),
            "3 days ago - Set Restart=on-failure."
        );
        assert_eq!(
            dated_passage("No date here", "Actual instructions."),
            "Actual instructions."
        );
    }

    #[test]
    fn a_short_text_is_returned_whole() {
        assert_eq!(cut("short and sweet", 180), "short and sweet");
        let long = "word ".repeat(100);
        let out = cut(&long, 50);
        assert!(out.chars().count() <= 51 && out.ends_with('…'), "{out}");
        assert_eq!(cut("中文", 0), "");
        assert_eq!(cut("中文", 1), "…");
        assert_eq!(cut("unbrokenidentifier", 5).chars().count(), 5);
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
        assert!(
            passage(
                "orion calibration",
                "*[dropped by focus: orion calibration]*\n\nUnrelated prose.",
                PASSAGE_CHARS
            )
            .is_none()
        );
        assert_eq!(
            coverage("site:example.com -python sqlite wal", "SQLite WAL"),
            1.0
        );
        let chinese =
            "Dockerfile 使用 FROM 指定基础镜像，使用 COPY 复制文件，使用 RUN 执行构建命令。";
        assert!(passage("Dockerfile 中文构建命令", chinese, PASSAGE_CHARS).is_some());

        let code = "# Nginx upstream configuration\n\nThis guide explains Nginx upstream configuration.\n\n```nginx\nlocation /api/ {\n    proxy_pass http://backend/;\n}\n```";
        let got = passage("nginx upstream configuration", code, PASSAGE_CHARS).unwrap();
        assert!(
            got.contains("location /api/ {\n    proxy_pass http://backend/;\n}"),
            "{got}"
        );
        assert!(
            !got.contains("```"),
            "fences cannot leak into the surrounding result list"
        );
        let table = "# Aeron prices\n\n| Size | USD |\n| --- | --- |\n| B | 950 |";
        let got = passage("aeron price", table, PASSAGE_CHARS).unwrap();
        assert!(
            got.contains("| Size | USD |\n| --- | --- |\n| B | 950 |"),
            "{got}"
        );
        assert!(
            passage("aeron price", table, 20).is_none(),
            "no unlabeled table values"
        );
        assert!(
            passage(
                "nginx upstream",
                "# Other\n\n```sh\necho unrelated\n```",
                PASSAGE_CHARS
            )
            .is_none()
        );
        let steps = "# Brake pad replacement\n\n1. Remove the wheel and retaining pin.\n2. Reset the pistons before fitting new brake pads.";
        let got = passage("how to replace brake pads", steps, PASSAGE_CHARS).unwrap();
        assert!(
            got.contains("Remove the wheel") && got.contains("Reset the pistons"),
            "{got}"
        );
        let guide = "# Install Orion on Linux\n\nLearn how to install Orion on Linux in this guide.\n\n## Installation\n\n```sh\norion install\norion use stable\n```";
        assert!(
            passage("how to install Orion on Linux", guide, PASSAGE_CHARS)
                .unwrap()
                .contains("orion use stable")
        );
        let directions = format!(
            "# Make a starter\n\n## Ingredients\n\n- Flour and water\n\n## Directions\n\n1. Mix 45 grams each flour and water. {}",
            "Keep stirring until the mixture is smooth. ".repeat(15)
        );
        let got = passage("how to make a starter", &directions, PASSAGE_CHARS).unwrap();
        assert!(
            got.contains("Mix 45 grams") && got.chars().count() <= PASSAGE_CHARS,
            "{got}"
        );
        let console = "```sh\n$ tool install \\\n  --stable\nDownloading tool-linux.tar.gz\nFinished\n$ tool use stable\nNow using stable\n```";
        let got = command_excerpt(console).unwrap();
        assert_eq!(got, "$ tool install \\\n  --stable\n…\n$ tool use stable");
        assert!(command_excerpt("```sh\nprintf '$value'\n```").is_none());
    }
}
