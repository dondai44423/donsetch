//! Result rendering for the search surface: the CLI/CLI-quiet
//! markdown list, the compact MCP evidence list, and the
//! structuredContent metadata. Pure functions over a finished
//! `SearchOutcome`; no engine/egress code lives here.

use serde_json::{Value, json};

use super::SearchOutcome;
use super::rank;

/// Snippet budget for the markdown list. 120 cut mid-phrase far
/// too often : the detail that distinguishes two results sat just
/// past the cut, and the agent paid a whole fetch to learn what the
/// snippet nearly said. 200 is where a snippet reliably carries one
/// complete claim; the 300 the JSON keeps is past diminishing
/// returns at ~45 tokens per result.
const SNIPPET_CHARS: usize = 200;

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
    for (i, r) in out.results.iter().enumerate() {
        let host = rank::host_of(&r.url);
        md.push_str(&format!("{}. **{}** : {}\n", i + 1, r.title, host));
        if !r.snippet.is_empty() {
            let snip = clip_snippet(&r.snippet, SNIPPET_CHARS);
            md.push_str(&format!("   {snip}\n"));
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
                md.push_str(&format!("   {}\n", r.url));
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
    if out.weak {
        md.push_str("\n*weak results: low cross-engine consensus : treat with care*\n");
    }
    // Zero hits is a success-shaped answer with nothing in it :
    // tell the agent which levers exist instead of leaving it
    // staring at an empty list.
    if out.results.is_empty() {
        md.push_str(
            "\n*0 results : try a simpler query, a different intent (news/code/paper), \
or add an API-key provider (`donsetch keys add`)*\n",
        );
    }
    let source = out.provider.as_deref().unwrap_or("local engine");
    md.push_str(&format!(
        "\n*{} results in {}ms via {}*\n",
        out.results.len(),
        out.elapsed.as_millis(),
        source
    ));
    // v3: degraded engines are named, never silently fewer. A merge
    // built while engines were down must never pass as full-strength.
    let failed: Vec<String> = out
        .report
        .iter()
        .filter(|r| r.status != "ok")
        .map(|r| format!("{}: {}", r.engine, r.status))
        .collect();
    if !failed.is_empty() {
        md.push_str(&format!(
            "*degraded: {}/{} engines ok ({}) : results may skew*\n",
            out.report.len() - failed.len(),
            out.report.len(),
            failed.join(", ")
        ));
    }
    if handles.is_some() && !out.results.is_empty() {
        md.push_str("*fetch results by their S-handle (raw urls in the result metadata)*\n");
    }
    md
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

    for (index, result) in out.results.iter().enumerate() {
        let reference = handles
            .and_then(|items| items.get(index))
            .map(String::as_str)
            .unwrap_or(&result.url);
        let host = rank::host_of(&result.url);
        markdown.push_str(&format!(
            "{}. {reference} · {} : {host}",
            index + 1,
            result.title
        ));
        // Report how many search-index families returned this URL,
        // using the same count as ranking. This is retrieval agreement,
        // not independent corroboration of the page's claims.
        let families = rank::family_count(result);
        if let Some(hint) = hints.get(index).and_then(|hint| hint.as_deref()) {
            markdown.push(' ');
            markdown.push_str(hint);
        }
        markdown.push_str(&format!(
            " · {} index {}",
            families,
            if families == 1 { "family" } else { "families" }
        ));
        markdown.push('\n');
        if !result.snippet.is_empty() {
            markdown.push_str("   ");
            markdown.push_str(&clip_snippet(&result.snippet, SNIPPET_CHARS));
            markdown.push('\n');
        }
    }

    if out.results.is_empty() {
        markdown.push_str("No results. Retry once with a materially different formulation.\n");
    } else if out.weak {
        markdown.push_str("Weak results : low cross-index agreement.\n");
    }

    let unavailable = out
        .report
        .iter()
        .filter(|report| report.status != "ok")
        .count();
    if unavailable > 0 {
        markdown.push_str(&format!(
            "Degraded retrieval : {}/{} backends available.\n",
            out.report.len() - unavailable,
            out.report.len()
        ));
    }

    markdown.trim_end().to_string()
}

/// structuredContent metadata.
/// The egress label for the machine meta: the known trio pass
/// through, anything else is a proxy chain.
fn egress_label(raw: &str) -> String {
    match raw {
        "direct" | "byok" | "ghost" => raw.to_string(),
        _ => "proxy".to_string(),
    }
}

pub fn render_meta(out: &SearchOutcome) -> Value {
    json!({
        "intent": format!("{:?}", out.intent),
        "weak": out.weak,
        "cached": out.cached,
        "elapsed_ms": out.elapsed.as_millis() as u64,
        "provider": out.provider,
        "rerank": if out.reranked { "on" } else { "off (RRF+BM25 fallback)" },
        "results": out.results.iter().map(|r| {
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
                "url": r.url,
                "snippet": r.snippet.chars().take(300).collect::<String>(),
                "score": (r.score * 1000.0).round() / 1000.0,
                "consensus": rank::family_count(r),
                "engines": seen_engines,
            })
        }).collect::<Vec<_>>(),
        "engines": out.report.iter().map(|r| {
            let mut report = json!({
                "engine": r.engine, "status": r.status, "hits": r.hits, "ms": r.ms,
                "egress": egress_label(&r.egress),
            });
            if let Some(profile) = &r.profile { report["profile"] = json!(profile); }
            report
        }).collect::<Vec<_>>(),
    })
}
