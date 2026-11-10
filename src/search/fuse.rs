//! Fusion: what the answer set itself proves.
//!
//! Two independent signals, kept apart until the last step:
//!
//! - **The family vote** (`rank::merge`) — how many independent indexes returned
//!   a URL and how highly. It knows nothing about text, and that is the point:
//!   three engines on one index are one opinion, not three.
//! - **The lexical evidence** (this module) — BM25+ over the merged candidates,
//!   with the query's own terms and the field a match landed in.
//!
//! Neither is enough alone. The vote alone ranks an arXiv front page above the
//! paper whose title is the query (`intitle:"Attention Is All You Need" arxiv`
//! returned nothing after the operator filter, because the paper sat beyond the
//! twelfth slot); the lexical score alone ranks a lone source's keyword hit above
//! a page three families agree on. The blend is linear and the weight is
//! measured, not assumed.

use std::collections::{HashMap, HashSet};

use super::contract::canonical_url;
use super::query::{self, QueryFacts};
use super::rank::{Merged, host_of};

/// BM25+ saturation and length normalisation.
pub const K1: f64 = 1.2;
pub const B: f64 = 0.75;
/// The `+` in BM25+: a present term pays `1 + DELTA`, so one occurrence in a
/// long field still counts. Absent terms contribute nothing — the variant that
/// adds the floor to every term would score a page that matches none of them.
pub const DELTA: f64 = 1.0;

/// Field weights. A title match is worth three snippets; a URL match one and a
/// half; the host alone, one.
const W_TITLE: f64 = 3.0;
const W_SNIPPET: f64 = 1.0;
const W_URL: f64 = 1.5;
const W_HOST: f64 = 1.0;
const W_TOTAL: f64 = W_TITLE + W_SNIPPET + W_URL + W_HOST;

/// At most this many results per host, however good they are.
pub const PER_HOST: usize = 2;

/// The most expansion terms RM3 may add.
pub const RM3_TERMS: usize = 12;

/// How much of the top vote the lexical evidence may move a candidate by —
/// **at most**. Measured: a 60% blend re-promoted SEO restatements of the query
/// over the pages the engines already rank well (`capital of australia` →
/// `insideaustraliatravel.com/faqs/australia-capital` above Wikipedia), because
/// BM25 rewards a page for repeating the query's words and the vote encodes what
/// three indexes have already learned about the page. The vote leads; evidence
/// breaks ties inside this window and nothing more.
pub const TIE_WINDOW: f64 = 0.05;

/// BM25+ over one field of the whole candidate set, with each query term's own
/// weight (a quoted phrase counts for more than a word).
///
/// `texts` is that field for every candidate, in the same order as the results.
/// The set is its own corpus: a term's idf is how rare it is *among the answers
/// this query got*, which is exactly the evidence a metasearch has.
fn field_scores(terms: &[(String, f64)], texts: &[&str], weight: f64) -> Vec<f64> {
    let n = texts.len();
    let lens: Vec<f64> = texts.iter().map(|t| token_len(t)).collect();
    let avg = (lens.iter().sum::<f64>() / n as f64).max(1e-9);
    let mut out = vec![0.0; n];
    for (term, qweight) in terms {
        let present: Vec<bool> = texts.iter().map(|t| query::matches(term, t)).collect();
        let df = present.iter().filter(|p| **p).count() as f64;
        if df == 0.0 {
            continue;
        }
        // Never negative: a term in every document carries no evidence, but it
        // must not invert the scale.
        let idf = (1.0 + (n as f64 - df + 0.5) / (df + 0.5)).ln();
        for (i, hit) in present.iter().enumerate() {
            if !hit {
                continue;
            }
            let tf = 1.0 + DELTA;
            let denom = tf + K1 * (1.0 - B + B * lens[i] / avg);
            out[i] += qweight * weight * idf * (tf * (K1 + 1.0)) / denom;
        }
    }
    out
}

/// The visible length of a field: words, not bytes, so a Japanese title is not
/// "longer" than an English one of the same word count.
fn token_len(text: &str) -> f64 {
    let words = text
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .count();
    (words.max(1)) as f64
}

/// BM25+ per candidate over title, snippet, URL and host, normalised to the
/// weighted total so the number does not depend on how many terms were asked.
pub fn lexical(facts: &QueryFacts, items: &[Merged]) -> Vec<f64> {
    let terms: Vec<(String, f64)> = facts
        .weighted_terms()
        .map(|(t, w)| (t.to_string(), w))
        .collect();
    lexical_with(&terms, items)
}

/// BM25+ over the candidates with an explicit term list: what RM3 hands back,
/// and what the query means before it is expanded.
pub fn lexical_with(terms: &[(String, f64)], items: &[Merged]) -> Vec<f64> {
    let [title, snippet, url, host] = fields_with(terms, items);
    (0..items.len())
        .map(|i| {
            (W_TITLE * title[i] + W_SNIPPET * snippet[i] + W_URL * url[i] + W_HOST * host[i])
                / W_TOTAL
        })
        .collect()
}

/// BM25+ per field, unweighted: `[title, snippet, url, host]`, one score per
/// candidate in each. What the field weights above blend, kept apart so the
/// learned ranker can weigh a title match differently from a host match.
pub fn fields(facts: &QueryFacts, items: &[Merged]) -> [Vec<f64>; 4] {
    let terms: Vec<(String, f64)> = facts
        .weighted_terms()
        .map(|(t, w)| (t.to_string(), w))
        .collect();
    fields_with(&terms, items)
}

fn fields_with(terms: &[(String, f64)], items: &[Merged]) -> [Vec<f64>; 4] {
    if items.is_empty() || terms.is_empty() {
        return std::array::from_fn(|_| vec![0.0; items.len()]);
    }
    let hosts: Vec<String> = items.iter().map(|m| host_of(&m.url)).collect();
    let titles: Vec<&str> = items.iter().map(|m| m.title.as_str()).collect();
    let snippets: Vec<&str> = items.iter().map(|m| m.snippet.as_str()).collect();
    let urls: Vec<&str> = items.iter().map(|m| m.url.as_str()).collect();
    let host_refs: Vec<&str> = hosts.iter().map(String::as_str).collect();
    [
        field_scores(terms, &titles, 1.0),
        field_scores(terms, &snippets, 1.0),
        field_scores(terms, &urls, 1.0),
        field_scores(terms, &host_refs, 1.0),
    ]
}

/// Expansion terms from the answer set (RM3), or `None` when the set is too
/// small, the query is a name, or the expansion would stop naming an anchor.
///
/// `w(t) = 0.5 · P(t | q) + 0.5 · P(t | R)`: the query's own weights, plus the
/// top documents' vocabulary weighted by how well each document answered.
fn rm3_terms(facts: &QueryFacts, items: &[Merged], evidence: &[f64]) -> Option<Vec<(String, f64)>> {
    if items.len() < 3 || facts.shape() == query::Shape::Identifier {
        return None;
    }
    let mut order: Vec<usize> = (0..items.len()).collect();
    order.sort_by(|a, b| evidence[*b].total_cmp(&evidence[*a]).then_with(|| a.cmp(b)));
    let top = &order[..order.len().min(8)];
    let mut freq: HashMap<String, f64> = HashMap::new();
    let mut mass = 0.0;
    for &i in top {
        let weight = evidence[i].max(0.0) + 1e-6;
        let text = format!("{} {}", items[i].title, items[i].snippet).to_lowercase();
        let mut seen: HashSet<String> = HashSet::new();
        for word in text.split(|c: char| !c.is_alphanumeric()) {
            if word.chars().count() < 3
                || !word.chars().all(|c| c.is_ascii_alphanumeric())
                || query::is_stop_word(word)
                || !seen.insert(word.to_string())
            {
                continue;
            }
            *freq.entry(word.to_string()).or_insert(0.0) += weight;
        }
        mass += weight;
    }
    if mass <= 0.0 {
        return None;
    }
    let mut weights: HashMap<String, f64> = HashMap::new();
    let qmass: f64 = facts.weighted_terms().map(|(_, w)| w).sum();
    if qmass > 0.0 {
        for (term, w) in facts.weighted_terms() {
            *weights.entry(term.to_string()).or_insert(0.0) += 0.5 * w / qmass;
        }
    }
    for (term, f) in freq {
        *weights.entry(term).or_insert(0.0) += 0.5 * f / mass;
    }
    let mut ranked: Vec<(String, f64)> = weights.into_iter().collect();
    ranked.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    ranked.truncate(RM3_TERMS);
    // An expansion that stops naming what was asked is not an expansion: the
    // query's anchors survive it, or the set is scored as it was.
    for anchor in facts.anchors() {
        if !ranked.iter().any(|(term, _)| term == anchor) {
            return None;
        }
    }
    Some(ranked)
}

/// The lexical score, expanded once by RM3 when the set supports it.
///
/// The answer set is the corpus: the rarest terms of the top documents
/// re-weight the query and the set is scored again. That is a synonym list
/// computed from the answers in front of us — no model, no download, and no
/// query variant the caller did not write.
pub fn reweight(facts: &QueryFacts, items: &[Merged]) -> Vec<f64> {
    let evidence = lexical(facts, items);
    match rm3_terms(facts, items, &evidence) {
        Some(terms) => lexical_with(&terms, items),
        None => evidence,
    }
}

/// Scale a signal to `0..=1` **against its own strongest candidate**, not
/// against its weakest.
///
/// Min-max normalisation was the first thing tried and it was wrong: it stretches
/// any spread, however small, to the full range, so a 0.01 difference in lexical
/// evidence became as decisive as a page that matched the whole query (measured:
/// the blend tied exactly on the arXiv row this module exists to fix). Dividing
/// by the maximum keeps the magnitudes — a candidate with a fifth of the best
/// score stays at 0.2.
pub fn normalize(values: &[f64]) -> Vec<f64> {
    let top = values.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    if values.is_empty() || !top.is_finite() || top <= 0.0 {
        return vec![0.0; values.len()];
    }
    values.iter().map(|v| (v / top).clamp(0.0, 1.0)).collect()
}

/// The fused score: the family vote, plus as much lexical evidence as a tie
/// break is worth.
///
/// Both signals are normalised against their own strongest candidate, then the
/// evidence may add at most [`TIE_WINDOW`] of the vote. Two candidates whose
/// vote is closer than that are, for ranking purposes, tied; their evidence
/// decides. A candidate the vote ranks clearly lower stays lower however well
/// its words match, which is the property the blend lost.
pub fn fuse(vote: &[f64], evidence: &[f64], window: f64) -> Vec<f64> {
    let vote = normalize(vote);
    let evidence = normalize(evidence);
    let w = window.clamp(0.0, 1.0);
    vote.iter().zip(&evidence).map(|(v, e)| v + w * e).collect()
}

/// The diversity pass over an ordered candidate list: at most [`PER_HOST`]
/// results per host, and one per canonical page. What does not fit is returned
/// as overflow, for a caller that may fill an empty slot with a page that is
/// itself about the query.
pub fn diversify(items: Vec<Merged>, max: usize) -> (Vec<Merged>, Vec<Merged>) {
    let mut hosts: HashMap<String, usize> = HashMap::new();
    let mut pages: HashSet<String> = HashSet::new();
    let mut kept: Vec<Merged> = Vec::with_capacity(max.min(items.len()));
    let mut overflow: Vec<Merged> = Vec::new();
    for item in items {
        if kept.len() >= max {
            overflow.push(item);
            continue;
        }
        let host = host_of(&item.url);
        let page = canonical_url(&item.url);
        let seen = hosts.entry(host).or_insert(0);
        if *seen >= PER_HOST || pages.contains(&page) {
            overflow.push(item);
            continue;
        }
        *seen += 1;
        pages.insert(page);
        kept.push(item);
    }
    (kept, overflow)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(url: &str, title: &str, snippet: &str) -> Merged {
        Merged {
            title: title.into(),
            url: url.into(),
            snippet: snippet.into(),
            sources: Vec::new(),
            score: 0.0,
            published: None,
            families: 1,
            from_page: false,
            tier: 1,
        }
    }

    #[test]
    fn a_title_match_outweighs_a_snippet_match() {
        let facts = QueryFacts::new("attention is all you need");
        let items = [
            m(
                "https://a.example/1",
                "Unrelated",
                "attention is all you need",
            ),
            m("https://b.example/2", "Attention Is All You Need", ""),
        ];
        let score = lexical(&facts, &items);
        assert!(score[1] > score[0], "{score:?}");
    }

    #[test]
    fn a_rare_term_outweighs_a_common_one() {
        let facts = QueryFacts::new("walrus banana");
        // "walrus" in one document, "banana" in both: the rare term separates.
        let items = [
            m("https://a.example/1", "banana walrus", ""),
            m("https://b.example/2", "banana", ""),
        ];
        let score = lexical(&facts, &items);
        assert!(score[0] > score[1], "{score:?}");
        assert!(score[1] > 0.0, "a common term still counts: {score:?}");
    }

    #[test]
    fn a_url_match_counts() {
        let facts = QueryFacts::new("spawn_blocking");
        let items = [
            m(
                "https://docs.rs/tokio/latest/tokio/task/fn.spawn_blocking.html",
                "spawn",
                "",
            ),
            m("https://example.com/other", "spawn", ""),
        ];
        let score = lexical(&facts, &items);
        assert!(score[0] > score[1], "{score:?}");
    }

    #[test]
    fn a_phrase_matches_only_as_a_run_of_words() {
        let facts = QueryFacts::new("\"attention is all you need\"");
        let items = [
            m("https://a.example/1", "need you all is attention", ""),
            m("https://b.example/2", "Attention Is All You Need", ""),
        ];
        let score = lexical(&facts, &items);
        assert!(score[1] > score[0], "{score:?}");
    }

    #[test]
    fn a_set_with_no_spread_carries_no_evidence() {
        // Nothing to divide by: no evidence rather than a fabricated tie-break.
        assert_eq!(normalize(&[0.0, 0.0, 0.0]), vec![0.0, 0.0, 0.0]);
        // Magnitudes are kept: a fifth of the best stays a fifth.
        let scaled = normalize(&[1.0, 0.2]);
        assert_eq!(scaled, vec![1.0, 0.2]);
        assert_eq!(normalize(&[2.0, 1.0]), vec![1.0, 0.5]);
    }

    #[test]
    fn rm3_expands_from_the_answers_and_keeps_the_query_terms() {
        let facts = QueryFacts::new("borrow checker");
        let items = vec![
            m(
                "https://doc.rust-lang.org/book/ch04-00-understanding-ownership.html",
                "Understanding Ownership",
                "The borrow checker enforces ownership and lifetimes.",
            ),
            m(
                "https://a.example/2",
                "Ownership and lifetimes",
                "The borrow checker rules for references.",
            ),
            m("https://b.example/3", "Rust book", "borrow checker chapter"),
        ];
        let evidence = lexical(&facts, &items);
        let terms = rm3_terms(&facts, &items, &evidence).expect("an expansion");
        let words: Vec<&str> = terms.iter().map(|(t, _)| t.as_str()).collect();
        assert!(words.contains(&"borrow"), "{words:?}");
        assert!(words.contains(&"checker"), "{words:?}");
        // The answers' own vocabulary is what the expansion adds.
        assert!(
            words.contains(&"lifetimes") || words.contains(&"ownership"),
            "{words:?}"
        );
        assert!(terms.len() <= RM3_TERMS, "{terms:?}");
    }

    #[test]
    fn rm3_is_not_asked_of_a_pool_too_small_to_be_a_corpus() {
        let facts = QueryFacts::new("borrow checker");
        let items = vec![
            m("https://a.example/1", "Borrow checker", ""),
            m("https://b.example/2", "Checker", ""),
        ];
        let evidence = lexical(&facts, &items);
        assert!(rm3_terms(&facts, &items, &evidence).is_none());
        // And reweight is exactly the plain lexical score there.
        assert_eq!(reweight(&facts, &items), lexical(&facts, &items));
    }

    #[test]
    fn rm3_is_not_asked_of_a_name() {
        let facts = QueryFacts::new("itertools::izip");
        let items: Vec<Merged> = (0..6)
            .map(|i| {
                m(
                    &format!("https://docs.rs/itertools/{i}"),
                    "izip",
                    "iterator",
                )
            })
            .collect();
        let evidence = lexical(&facts, &items);
        assert!(rm3_terms(&facts, &items, &evidence).is_none());
    }

    #[test]
    fn an_expansion_that_drops_the_anchor_is_refused() {
        // The query names SpaceX; the top documents never do, and every one of
        // them uses the same fourteen other words, so each of those outweighs the
        // anchor's query weight and fills the twelve slots. Expanding here would
        // score a set about the wrong subject.
        let facts = QueryFacts::new("SpaceX launch");
        let text = "launch window weather rocket fuel stage booster pad tower \
                    schedule crew payload orbit telemetry";
        let mut items = Vec::new();
        for i in 0..4 {
            items.push(m(&format!("https://a.example/{i}"), text, text));
        }
        let evidence = lexical(&facts, &items);
        assert!(rm3_terms(&facts, &items, &evidence).is_none());
    }

    #[test]
    fn evidence_breaks_a_tie_and_cannot_overturn_a_clear_lead() {
        // Two candidates a family ranked 2 and 3: their vote differs inside the
        // window, so the evidence decides.
        let vote = [0.90, 0.89];
        let evidence = [0.0, 1.0];
        let fused = fuse(&vote, &evidence, TIE_WINDOW);
        assert!(fused[1] > fused[0], "{fused:?}");
        // A clear lead is not overturned, however well the other one matches.
        // This is the property the even blend lost: `capital of australia`
        // handed rank 1 to a travel FAQ that repeats the query over Wikipedia.
        let vote = [1.0, 0.1];
        let evidence = [0.0, 1.0];
        let fused = fuse(&vote, &evidence, TIE_WINDOW);
        assert!(fused[0] > fused[1], "{fused:?}");
    }

    #[test]
    fn diversity_keeps_two_per_host_and_one_per_page() {
        let items = vec![
            m("https://a.example/1", "one", ""),
            m("https://a.example/2", "two", ""),
            m("https://a.example/3", "three", ""),
            m("https://b.example/1?utm_source=x", "one", ""),
            m("https://b.example/1", "one again", ""),
            m("https://c.example/1", "four", ""),
        ];
        let (kept, overflow) = diversify(items, 10);
        assert_eq!(
            kept.iter()
                .filter(|r| host_of(&r.url) == "a.example")
                .count(),
            2
        );
        // The same page under two tracking spellings is one page.
        assert_eq!(kept.len(), 4, "kept {kept:?}");
        assert_eq!(overflow.len(), 2, "overflow {overflow:?}");
    }

    #[test]
    fn diversity_honours_the_result_cap() {
        let items: Vec<Merged> = (0..6)
            .map(|i| m(&format!("https://h{i}.example/x"), "t", ""))
            .collect();
        let (kept, overflow) = diversify(items, 3);
        assert_eq!(kept.len(), 3);
        assert_eq!(overflow.len(), 3);
        assert_eq!(host_of(&kept[0].url), "h0.example");
        assert_eq!(host_of(&overflow[0].url), "h3.example");
    }
}
