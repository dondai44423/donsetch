//! Result rendering for the search surface: the CLI/CLI-quiet
//! markdown list, the compact MCP evidence list, and the
//! structuredContent metadata. Pure functions over a finished
//! `SearchOutcome`; no engine/egress code lives here.

use serde_json::{Value, json};

use super::SearchOutcome;
use super::rank;

/// Below this fraction (4/5) of the budget, a word-boundary cut
/// throws away more than it saves : see `clip_snippet`.
const CLIP_FLOOR_NUM: usize = 4;
const CLIP_FLOOR_DEN: usize = 5;

/// Trailing marks dropped before the ellipsis: each one JOINS
/// clauses, so ending on it reads as a typo rather than a cut.
///
/// Sentence terminators (. ! ? 。！？) are deliberately KEPT: a cut
/// that lands after one means the snippet ended at a complete
/// sentence, and saying so is worth more than tidiness. Stripping
/// them would make a clean ending look like a severed one.
/// CJK marks are the same codepoints in Chinese and Japanese, so
/// one list serves both: 、and ，join clauses, 《》【】「」（ open
/// spans. 。！？ are absent on purpose : they end sentences.
const CLIP_TRIM: &[char] = &[
    ',', ';', ':', '(', '[', '{', '/', '|', '…', '、', '，', '；', '：', '（', '「', '『', '《',
    '〈', '【', '〔', '［', '｛', '·', '／', '｜', '〜',
];

/// Truncate to `max` chars on a word boundary, marking the cut with
/// an ellipsis ONLY when text was actually dropped.
///
/// Trims back, never extends: extending to finish the straddling
/// word would make the output size unbounded by `max` (one long
/// token and a "200-char snippet" is 280), and the fragment dropped
/// is a partial word the agent cannot use anyway.
///
/// The 4/5 floor bounds the pathological case: a long URL, hash or
/// compound word straddling the boundary would otherwise back off to
/// almost nothing, which is worse than a mid-word cut the ellipsis
/// already flags.
pub(super) fn clip_snippet(s: &str, max: usize) -> String {
    // Materialized rather than iterated because the window is read
    // three ways: indexed (chars[max]), scanned BACKWARDS for the
    // last space, and sliced for the head. `Chars` cannot be
    // rewound, so an iterator version re-decodes UTF-8 from the
    // start once per pass : and `rposition` is not even available
    // on it (it needs ExactSizeIterator, which `Chars` is not),
    // leaving manual position bookkeeping. Decode once, index
    // freely.
    //
    // Char positions, not &str byte offsets, for the same reason:
    // the budget and the floor are counted in chars, so `rfind`'s
    // byte index would need converting before every comparison :
    // and mixing the two on multi-byte text is where UTF-8 bugs
    // breed.
    //
    // max + 1 and no further: the only index past the window we
    // inspect is chars[max], the "does the next char end a word?"
    // test. Collecting the whole string would allocate 4 bytes a
    // char for input we discard : BYOK snippets carry raw page
    // text and run to thousands of chars.
    let chars: Vec<char> = s.chars().take(max + 1).collect();
    if chars.len() <= max {
        return s.to_string();
    }
    // The char PAST the window decides whether the window already
    // ends cleanly. If it is whitespace, the last word inside is
    // whole and backing off would drop a complete word for nothing.
    let cut = if chars[max].is_whitespace() {
        max
    } else {
        match chars[..max].iter().rposition(|c| c.is_whitespace()) {
            Some(pos) if pos * CLIP_FLOOR_DEN >= max * CLIP_FLOOR_NUM => pos,
            _ => max,
        }
    };
    let mut head: String = chars[..cut].iter().collect();
    // trim_end_matches only slices : it is the `.to_string()` that
    // would copy. Truncating to the trimmed length shortens in
    // place instead, leaving one allocation for the whole function.
    let keep = head
        .trim_end_matches(|c: char| c.is_whitespace() || CLIP_TRIM.contains(&c))
        .len();
    head.truncate(keep);
    // Only whitespace left behind means nothing was really dropped;
    // an ellipsis there would promise content that does not exist.
    // Iterates the ORIGINAL string, not the bounded window: a
    // Vec capped at max + 1 cannot answer "is everything after
    // the cut whitespace?". `all` short-circuits on the first
    // non-whitespace, so this is O(1) in practice.
    if s.chars().skip(cut).all(char::is_whitespace) {
        return head;
    }
    format!("{head}…")
}

/// One line of retrieval state: every source that answered, with its item count,
/// and every source that did not, with the reason it gave.
///
/// This replaces the "weak results" flag and the "degraded N/M" line together.
/// A flag told the model how to feel about the list; the census tells it what
/// happened, which is the only thing that changes the next action. It names
/// off-query answers too — a 200 carrying ten unrelated items is a fact about
/// retrieval, not a success.
pub fn census(report: &[super::EngineReport]) -> String {
    // A source the plan never asked did nothing: naming it spends the reader's
    // context on a non-event. A source the ledger is resting is a standing
    // condition, not news about this query, so it is counted, not named (a dead
    // engine would otherwise be named on every search). Every source that was
    // asked is named with what it did; the JSON census names them all.
    let states = source_states(report);
    let resting = states.iter().filter(|s| s.state == "rested").count();
    let mut parts: Vec<String> = states
        .into_iter()
        .filter(|s| !matches!(s.state, "not asked" | "rested"))
        .map(|s| match s.state {
            "answered" => format!("{} {}", s.id, s.items),
            state if s.items > 0 => format!("{} {} {state}", s.id, s.items),
            state => format!("{} — {state}", s.id),
        })
        .collect();
    if resting > 0 {
        parts.push(format!("{resting} resting"));
    }
    if parts.is_empty() {
        return "sources: none".into();
    }
    format!("sources: {}", parts.join(" · "))
}

/// What one source did in one search.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceLine {
    pub id: String,
    /// Items the source returned.
    pub items: usize,
    /// The raw outcome (`ok`, `no-results`, `403`, `rate-limited:3219`,
    /// `late`, ...), stable since 5.1: `blocked:` is shortened to its code.
    pub status: String,
    /// The census word for it: `answered`, `empty`, `off`, `rested`,
    /// `not asked`, `late` or `failed`.
    pub state: &'static str,
}

/// Deduped per-source retrieval state, one line per source.
///
/// The one source of truth behind the text census and the JSON `sources` field.
/// A source that answered once and failed on a retry still counts as answered.
pub fn source_states(report: &[super::EngineReport]) -> Vec<SourceLine> {
    use std::collections::BTreeMap;
    let mut best: BTreeMap<&str, &super::EngineReport> = BTreeMap::new();
    // The most informative attempt speaks for the source: an answer (even an
    // empty or off-query one) beats a scheduling outcome, so a source asked
    // once and skipped once is not reported as "not asked".
    for entry in report {
        let slot = best.entry(entry.engine.as_str()).or_insert(entry);
        if rank_state(&entry.status) > rank_state(&slot.status) {
            *slot = entry;
        }
    }
    best.into_iter()
        .map(|(name, entry)| SourceLine {
            id: name.to_string(),
            items: entry.hits,
            status: entry
                .status
                .strip_prefix("blocked:")
                .unwrap_or(&entry.status)
                .to_string(),
            state: state_word(&entry.status),
        })
        .collect()
}

/// How much a status says about a source, from "we never asked" to "it refused".
pub(crate) fn rank_state(status: &str) -> u8 {
    match status.split(':').next().unwrap_or(status) {
        "not-asked" | "pacing-timeout" | "budget-skipped" => 0,
        "rested" => 1,
        "paced" => 1,
        "late" => 2,
        // A wall is worth saying — but an answer, even an empty one, is what
        // the source actually did.
        "off-query" | "no-results" | "empty-parse" | "thin" => 4,
        "ok" | "cached" => 5,
        _ => 3,
    }
}

/// The census vocabulary: `answered`, `empty`, `off`, `rested`, `not asked`,
/// `failed`, and `late` for a lane the scheduler cut. A source that was never
/// asked is never a failure, and neither is one resting under a limit it
/// published (`rate-limited:<secs>`); the raw status keeps the detail.
fn state_word(status: &str) -> &'static str {
    let head = status.split(':').next().unwrap_or(status);
    match head {
        "ok" | "cached" | "thin" | "answered" => "answered",
        "no-results" | "empty" | "empty-parse" => "empty",
        "off-query" => "off",
        "rested" | "rate-limited" => "rested",
        "paced" => "paced",
        "not-asked" | "pacing-timeout" | "budget-skipped" => "not asked",
        "late" => "late",
        _ => "failed",
    }
}

/// Markdown rendering for the MCP/CLI surface.
pub fn render_markdown(
    out: &SearchOutcome,
    query: &str,
    handles: Option<&[String]>,
    hints: &[Option<String>],
) -> String {
    // Search answers ONE question: "what should I fetch?"
    // Snippets carry just enough to decide : content is
    // the fetch tool's job.
    let mut md = format!("# Search: {query}\n\n");
    // C5: instant answers sit ABOVE the organic list, never inside
    // it. Source URL always present; text is byte-derived.
    if let Some(ans) = &out.instant {
        let kind = match ans.kind {
            "knowledge" => "Knowledge",
            "answer" => "Instant answer",
            _ => "Featured snippet",
        };
        let title = if ans.title.is_empty() {
            String::new()
        } else {
            format!(" — {}", ans.title)
        };
        md.push_str(&format!(
            "## {kind} ({}){title}\n{}\n{}\n\n",
            ans.engine,
            clip_snippet(&ans.text, 400),
            ans.url
        ));
    }
    let snippets = evidence_snippets(&out.results);
    for (i, r) in out.results.iter().enumerate() {
        let host = rank::host_of(&r.url);
        md.push_str(&format!("{}. **{}** : {}\n", i + 1, r.title, host));
        if !r.snippet.is_empty() {
            let snip = &snippets[i];
            md.push_str(&format!("   {}\n", snip.replace('\n', "\n   ")));
        }
        // v3 handles: a random S-handle replaces the
        // raw URL, saving 80+ tokens per result.
        match handles {
            Some(hs) if let Some(h) = hs.get(i) => {
                // v3 F2: a known-walled domain carries its route
                // cost : pick a faster source or budget time
                // BEFORE spending the fetch.
                match hints.get(i).and_then(|h| h.as_deref()) {
                    Some(hint) => md.push_str(&format!("   {h} {hint}\n")),
                    None => md.push_str(&format!("   {h}\n")),
                }
            }
            _ => {
                md.push_str(&format!("   {}\n", evidence_url(&r.url)));
            }
        }
        // Provenance, text-side. Which engines returned a URL is the
        // signal that separates two equally plausible results: three
        // independent indexes agreeing usually means canonical, a
        // lone vertical hit often means tangential. Until now it
        // existed only in structuredContent, so a client that drops
        // that field could not tell the two apart.
        //
        // NAMES, not a count: `consensus` in the JSON is
        // sources.len(), which double-counts an engine that returned
        // the URL at two ranks (live: ddg, yahoo, yahoo, brave = 4
        // for 3 engines). Ranking counts index FAMILIES instead, so
        // deduped names are both cheaper to read and more honest
        // than the number : and they say WHICH source, which a count
        // never can.
        let mut seen_engines: Vec<&str> = Vec::new();
        for (engine, _) in &r.sources {
            if !seen_engines.contains(&engine.as_str()) {
                seen_engines.push(engine);
            }
        }
        if !seen_engines.is_empty() {
            // 2dp, not the JSON's 3: this is a blended heuristic, and
            // 0.831 reads like a measurement.
            md.push_str(&format!(
                "   engines: {} · score: {:.2}\n",
                seen_engines.join(", "),
                r.score
            ));
        }
    }
    if out.results.is_empty() {
        md.push_str(
            "\n*0 results: retry once before rephrasing — a slow source is not a dry \
query — then try a simpler query, a different intent (news/code/paper), or an \
API-key provider (`donsetch keys add`)*\n",
        );
    } else if out.results.len() < 3 {
        md.push_str("\n*thin answer: a retry asks the sources that were late*\n");
    }
    let source = out.provider.as_deref().unwrap_or("local search");
    md.push_str(&format!(
        "\n*{} results in {}ms via {}*\n",
        out.results.len(),
        out.elapsed.as_millis(),
        source
    ));
    // The census is the honesty layer: which sources answered, with how many
    // items, and what every source that did not answer actually said. It is not
    // a "weak results" mood -- it is the retrieval state, which is what changes
    // the next action (rephrase, wait, or fetch anyway).
    md.push_str(&format!("*{}*\n", census(&out.report)));
    if handles.is_some() && !out.results.is_empty() {
        md.push_str("*fetch results by their S-handle (raw urls in the result metadata)*\n");
    }
    md
}

/// A title without the site name the row already shows as its host:
/// "Attention Is All You Need - Wikipedia" on en.wikipedia.org is the title and
/// the host said twice. Only a trailing, separator-set part that is contained in
/// the host's own name is dropped, and never down to nothing.
fn display_title(title: &str, host: &str) -> String {
    const SEPARATORS: &[&str] = &[" - ", " | ", " · ", " – ", " — ", " :: "];
    let key = |s: &str| {
        s.chars()
            .filter(char::is_ascii_alphanumeric)
            .collect::<String>()
            .to_ascii_lowercase()
    };
    let host_key = key(host);
    let cut = SEPARATORS
        .iter()
        .filter_map(|sep| title.rfind(sep).map(|at| (at, sep.len())))
        .max_by_key(|(at, _)| *at);
    if let Some((at, len)) = cut {
        let (head, tail) = (title[..at].trim_end(), title[at + len..].trim());
        let tail_key = key(tail);
        if head.chars().count() >= 3 && tail_key.len() >= 4 && host_key.contains(&tail_key) {
            return head.to_string();
        }
    }
    title.to_string()
}

/// What the snippet of the result at `position` (0-based) may show: a passage
/// read from the page, or one of the lead results, takes the passage budget;
/// the rest the snippet budget.
pub(super) fn snippet_limit(position: usize, result: &super::rank::Merged) -> usize {
    if result.from_page {
        super::evidence::PASSAGE_CHARS
    } else {
        super::evidence::budget(position)
    }
}

/// A readable URL with the same parsed destination. Only encoded Unicode
/// letters and digits are expanded; reserved delimiters and escapes stay put.
pub fn evidence_url(raw: &str) -> String {
    let Ok(original) = url::Url::parse(raw) else {
        return raw.to_string();
    };
    if !matches!(original.scheme(), "http" | "https") {
        return raw.to_string();
    }
    let Some(authority) = raw.find("://").map(|at| at + 3) else {
        return raw.to_string();
    };
    let start = raw[authority..]
        .find(['/', '?', '#'])
        .map_or(raw.len(), |at| authority + at);
    let bytes = raw.as_bytes();
    let mut out = String::with_capacity(raw.len());
    out.push_str(&raw[..start]);
    let mut i = start;
    while i < bytes.len() {
        let decoded = (|| {
            let byte = |at: usize| {
                if bytes.get(at) != Some(&b'%') {
                    return None;
                }
                let hex = std::str::from_utf8(bytes.get(at + 1..at + 3)?).ok()?;
                u8::from_str_radix(hex, 16).ok()
            };
            let first = byte(i)?;
            let width = match first {
                0xc2..=0xdf => 2,
                0xe0..=0xef => 3,
                0xf0..=0xf4 => 4,
                _ => return None,
            };
            let mut buf = [0u8; 4];
            for (n, slot) in buf[..width].iter_mut().enumerate() {
                *slot = byte(i + n * 3)?;
            }
            let c = std::str::from_utf8(&buf[..width]).ok()?.chars().next()?;
            c.is_alphanumeric().then_some((c, width * 3))
        })();
        if let Some((c, len)) = decoded {
            out.push(c);
            i += len;
        } else {
            let c = raw[i..].chars().next().expect("character boundary");
            out.push(c);
            i += c.len_utf8();
        }
    }
    if url::Url::parse(&out).ok().as_ref() == Some(&original) {
        out
    } else {
        raw.to_string()
    }
}

/// Share a 1,600-byte snippet envelope across the visible answer. Character
/// ceilings still apply, and the lead results receive the larger share. This
/// limits multibyte text without a runtime tokenizer; the release gate also
/// measures the complete evidence with its real tokenizer.
pub(super) fn evidence_snippets(results: &[super::rank::Merged]) -> Vec<String> {
    let snippets: Vec<String> = results
        .iter()
        .enumerate()
        .map(|(i, r)| {
            let date = r
                .published
                .as_deref()
                .and_then(|raw| raw.get(..10))
                .filter(|date| super::contract::day_of(date).is_some());
            let text = match date {
                Some(date) if !r.snippet.starts_with(date) => format!("{date} · {}", r.snippet),
                _ => r.snippet.clone(),
            };
            clip_snippet(&text, snippet_limit(i, r))
        })
        .collect();
    let mut remaining = 1600usize;
    let mut sizes = vec![0usize; snippets.len()];
    loop {
        let active: Vec<usize> = (0..snippets.len())
            .filter(|&i| sizes[i] < snippets[i].len())
            .collect();
        if active.is_empty() || remaining == 0 {
            break;
        }
        let weight: usize = active.iter().map(|&i| snippet_limit(i, &results[i])).sum();
        let allowance = remaining;
        for i in active {
            let share = (allowance * snippet_limit(i, &results[i]) / weight)
                .max(1)
                .min(remaining)
                .min(snippets[i].len() - sizes[i]);
            sizes[i] += share;
            remaining -= share;
        }
    }
    snippets
        .into_iter()
        .zip(sizes)
        .map(|(s, bytes)| {
            if s.len() <= bytes {
                return s;
            }
            if bytes < '…'.len_utf8() {
                return String::new();
            }
            let mut end = bytes - '…'.len_utf8();
            while !s.is_char_boundary(end) {
                end -= 1;
            }
            clip_snippet(&s, s[..end].chars().count())
        })
        .collect()
}

/// One result on the compact surface: a line naming it (handle, title, host,
/// kind, route hint, and how many independent indexes returned it when more than
/// one did) and the snippet under it. A single index is the unremarkable case and
/// says nothing.
pub(super) fn compact_row(
    number: usize,
    reference: &str,
    result: &super::rank::Merged,
    hint: Option<&str>,
) -> String {
    let host = rank::host_of(&result.url);
    let mut row = format!(
        "{number}. {reference} · {} : {host}",
        display_title(&result.title, &host)
    );
    let kind = rank::source_type(&result.url);
    if kind != "web" {
        row.push_str(&format!(" · {kind}"));
    }
    if let Some(hint) = hint {
        row.push(' ');
        row.push_str(hint);
    }
    let indexes = rank::family_count(result);
    if indexes > 1 {
        row.push_str(&format!(" · {indexes} indexes"));
    }
    row.push('\n');
    if !result.snippet.is_empty() {
        let limit = snippet_limit(number.saturating_sub(1), result);
        row.push_str("   ");
        if result.from_page {
            row.push_str("page: ");
        }
        row.push_str(&clip_snippet(&result.snippet, limit).replace('\n', "\n   "));
        row.push('\n');
    }
    row
}

/// Compact MCP evidence surface. Rank already communicates the ordering
/// decision, while per-engine scores and timings remain available as client
/// diagnostics. Keep only evidence and state that can alter the next action.
pub fn render_compact_markdown(
    out: &SearchOutcome,
    heading: &str,
    handles: Option<&[String]>,
    hints: &[Option<String>],
) -> String {
    let mut markdown = String::new();
    if !heading.is_empty() {
        markdown.push_str(heading);
        markdown.push('\n');
    }
    if let Some(ans) = &out.instant {
        // Compact surface: one line. Full text lives in structuredContent.
        let label = match ans.kind {
            "knowledge" => "knowledge",
            "answer" => "instant",
            _ => "featured",
        };
        markdown.push_str(&format!(
            "Instant ({label} via {}): {} · {}\n",
            ans.engine,
            clip_snippet(&ans.text, 160),
            ans.url
        ));
    }

    let snippets = evidence_snippets(&out.results);
    for (index, original) in out.results.iter().enumerate() {
        let mut result = original.clone();
        result.snippet.clone_from(&snippets[index]);
        let readable_url = evidence_url(&result.url);
        let reference = handles
            .and_then(|items| items.get(index))
            .map(String::as_str)
            .unwrap_or(&readable_url);
        let hint = hints.get(index).and_then(|hint| hint.as_deref());
        markdown.push_str(&compact_row(index + 1, reference, &result, hint));
    }

    if out.results.is_empty() {
        markdown.push_str("No results. Retry once with a materially different formulation.\n");
    }
    markdown.push_str(&format!("{}\n", census(&out.report)));

    markdown.trim_end().to_string()
}

/// structuredContent metadata.
/// The egress label for the machine meta: the known labels
/// pass through, anything else is a proxy chain.
fn egress_label(raw: &str) -> String {
    match raw {
        "direct" | "byok" | "cache" => raw.to_string(),
        _ => "proxy".to_string(),
    }
}

pub fn render_meta(out: &SearchOutcome) -> Value {
    let snippets = evidence_snippets(&out.results);
    // What a caller should know without reading the census: whether the list is
    // an answer, and what to do when it is not. `ok` stays true for an empty
    // answer ("nothing found" is a result), so these three fields carry the
    // distinction an agent branches on.
    let answered = out
        .report
        .iter()
        .filter(|r| matches!(r.status.as_str(), "ok" | "cached" | "thin"))
        .count();
    let underfilled = out.results.len() < 3;
    let degraded = out.quality.degraded();
    let next_action = if out.results.is_empty() {
        Some("retry once: no source answered inside the budget; the second call usually lands them")
    } else if underfilled {
        Some("retry once, or rephrase: fewer results than a list to choose from")
    } else if degraded {
        Some("limited independent evidence; retry for confirmation")
    } else {
        None
    };
    let instant = out.instant.as_ref().map(|ans| {
        json!({
            "kind": ans.kind,
            "title": ans.title,
            "text": ans.text,
            "url": ans.url,
            "engine": ans.engine,
        })
    });
    json!({
        "intent": format!("{:?}", out.intent),
        "cached": out.cached,
        "requests": out.requests,
        "confidence": out.quality.confidence,
        "cache_age_ms": out.cache_age.map(|age| age.as_millis() as u64),
        "independent_families": out.quality.families,
        "elapsed_ms": out.elapsed.as_millis() as u64,
        "stage_ms": if out.stage_ms.is_empty() {
            serde_json::Value::Null
        } else {
            serde_json::Value::Object(out
                .stage_ms
                .iter()
                .map(|(k, v)| {
                    (
                        k.to_string(),
                        serde_json::json!(*v as u64),
                    )
                })
                .collect::<serde_json::Map<String, serde_json::Value>>())
        },
        "provider": out.provider,
        "filled": !out.results.is_empty(),
        "underfilled": underfilled,
        "degraded": degraded,
        "answered_sources": answered,
        "next_action": next_action,
        "rerank": if out.reranked { "on" } else { "off (no runtime model)" },
        "instant": instant,
        "results": out.results.iter().enumerate().map(|(position, r)| {
            // Named sources (deduped: an engine surfacing a URL at
            // two ranks is one opinion for the list, exactly like
            // the markdown surface). Values stay engine names.
            let mut seen_engines: Vec<&str> = Vec::new();
            for (e, _) in &r.sources {
                if !seen_engines.contains(&e.as_str()) {
                    seen_engines.push(e);
                }
            }
            json!({
                "title": r.title,
                "url": evidence_url(&r.url),
                "snippet": snippets[position],
                "score": (r.score * 1000.0).round() / 1000.0,
                "consensus": rank::family_count(r),
                "engines": seen_engines,
            })
        }).collect::<Vec<_>>(),
        "engines": out.report.iter().map(|r| {
            let mut report = json!({
                "engine": r.engine, "status": r.status, "hits": r.hits, "ms": r.ms,
                "egress": egress_label(&r.egress), "cache_age_ms": r.cache_age_ms, "requests": r.requests,
            });
            if let Some(profile) = &r.profile { report["profile"] = json!(profile); }
            report
        }).collect::<Vec<_>>(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search::rank::Merged;

    fn result(title: &str, url: &str, snippet: &str) -> Merged {
        Merged {
            title: title.into(),
            url: url.into(),
            snippet: snippet.into(),
            sources: Vec::new(),
            score: 1.0,
            published: None,
            families: 1,
            from_page: false,
            tier: 0,
        }
    }

    #[test]
    fn a_site_name_the_host_already_says_is_dropped_from_the_title() {
        let t = |title, host| display_title(title, host);
        assert_eq!(
            t("Attention Is All You Need - Wikipedia", "en.wikipedia.org"),
            "Attention Is All You Need"
        );
        assert_eq!(
            t(
                "Rust async runtime | GeeksforGeeks",
                "www.geeksforgeeks.org"
            ),
            "Rust async runtime"
        );
        // Not the host's name, so it is information: kept.
        assert_eq!(
            t(
                "Array.prototype.reduce() - JavaScript | MDN",
                "developer.mozilla.org"
            ),
            "Array.prototype.reduce() - JavaScript | MDN"
        );
        // Never down to nothing, and a short tail is not a name.
        assert_eq!(t("A - Wikipedia", "en.wikipedia.org"), "A - Wikipedia");
        assert_eq!(t("Go - Go", "go.dev"), "Go - Go");
    }

    #[test]
    fn a_row_names_agreement_only_when_there_is_some() {
        let mut r = result(
            "Tokio guide",
            "https://tokio.rs/guide",
            "Learn the runtime.",
        );
        let one = compact_row(1, "S1", &r, None);
        assert_eq!(
            one,
            "1. S1 · Tokio guide : tokio.rs\n   Learn the runtime.\n"
        );
        r.families = 3;
        assert!(
            compact_row(1, "S1", &r, Some("· ⚠ needs browser"))
                .starts_with("1. S1 · Tokio guide : tokio.rs · ⚠ needs browser · 3 indexes\n")
        );
    }

    #[test]
    fn a_page_passage_is_labelled_and_may_run_longer_than_a_teaser() {
        let long = "word ".repeat(60);
        let mut r = result("T", "https://example.com/a", long.trim());
        // Past the lead results a teaser takes the short budget.
        let teaser = compact_row(5, "S5", &r, None);
        r.from_page = true;
        let page = compact_row(5, "S5", &r, None);
        assert!(page.contains("   page: word"), "{page}");
        assert!(
            page.len() > teaser.len() + 100,
            "a passage has the longer limit"
        );
    }

    #[test]
    fn readable_evidence_urls_preserve_destinations_and_reserved_escapes() {
        let raw =
            "https://example.com/%E4%B8%AD%E6%96%87/%2F?q=%E6%97%A5%E6%9C%AC%26x%3Dy%25#%E7%AB%A0";
        let readable = evidence_url(raw);
        assert_eq!(
            readable,
            "https://example.com/中文/%2F?q=日本%26x%3Dy%25#章"
        );
        assert_eq!(
            url::Url::parse(raw).unwrap(),
            url::Url::parse(&readable).unwrap()
        );
        for unchanged in [
            "https://example.com/a%2Fb?q=a%20b%25",
            "https://example.com/%E2%80%AEabc",
            "https://example.com/%FF",
            "not a URL",
        ] {
            assert_eq!(evidence_url(unchanged), unchanged);
        }
    }

    #[test]
    fn the_lead_results_carry_more_evidence_and_the_surface_is_bounded() {
        let long = "evidence ".repeat(80);
        let row = |n| {
            compact_row(
                n,
                "S1",
                &result("T", "https://example.com/a", long.trim()),
                None,
            )
        };
        assert!(
            row(3).len() > row(4).len() + 100,
            "{} vs {}",
            row(3).len(),
            row(4).len()
        );
        // Exercise twelve capped snippets and long titles. This checks the
        // character envelope only; real tokenizer counts belong in the live
        // evidence audit, because scripts do not share a chars/token ratio.
        let title = "A long result title that names the page and then some more ".repeat(2);
        let out = SearchOutcome {
            results: (0..12)
                .map(|i| {
                    result(
                        &title,
                        &format!("https://host{i}.example.com/a/b/c"),
                        long.trim(),
                    )
                })
                .collect(),
            intent: crate::search::intent::Intent::Web,
            report: Vec::new(),
            cached: false,
            quality: Default::default(),
            cache_age: None,
            elapsed: std::time::Duration::ZERO,
            provider: None,
            reranked: false,
            instant: None,
            requests: None,
            stage_ms: Vec::new(),
        };
        let md = render_compact_markdown(&out, "", None, &[]);
        assert!(md.chars().count() <= 6_000, "unbounded evidence surface");
        for text in [
            "日本語の根拠を表示します。 ".repeat(80),
            "🦀 Unicode evidence. ".repeat(80),
        ] {
            let mut unicode = out.clone();
            for r in &mut unicode.results {
                r.snippet.clone_from(&text);
            }
            let visible = evidence_snippets(&unicode.results);
            assert!(visible.iter().map(String::len).sum::<usize>() <= 1600);
            assert!(visible[0].len() > visible[3].len());
            let meta = render_meta(&unicode);
            for (i, snippet) in visible.iter().enumerate() {
                assert_eq!(meta["results"][i]["snippet"], *snippet);
            }
        }
        let brief = vec![result("T", "https://example.com", "A complete factual claim."); 5];
        assert!(
            evidence_snippets(&brief)
                .iter()
                .all(|s| s == "A complete factual claim.")
        );
        let mut dated = out.clone();
        dated.results.truncate(1);
        dated.results[0].published = Some("2026-11-11T12:00:00Z".into());
        dated.results[0].snippet = "A dated event actually happened.".into();
        let shown = evidence_snippets(&dated.results);
        assert_eq!(shown[0], "2026-11-11 · A dated event actually happened.");
        assert!(render_compact_markdown(&dated, "", None, &[]).contains(&shown[0]));
        assert_eq!(render_meta(&dated)["results"][0]["snippet"], shown[0]);
        dated.results[0].published = Some("not a date".into());
        assert_eq!(
            evidence_snippets(&dated.results)[0],
            "A dated event actually happened."
        );
    }
}
