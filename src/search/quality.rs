//! Set-level judgement: what an answer *is*, once its items have been judged.
//!
//! The layer this replaces decided a source's fate per hit: a weighted coverage
//! test, then a guillotine — fewer than half the top ten admitted and the whole
//! answer was `OffQuery`, its items dropped and its source distrusted. Measured
//! on this build: a long query returned zero while DDG delivered ten results and
//! Yahoo twenty, thirty real results rejected by arithmetic; the lane then lost
//! trust, rested, and was skipped next time. Our own strictness decayed the only
//! lanes that were answering.
//!
//! Here admission is a **veto list** ([`contract::veto`]): a wrapper, a SERP, a
//! tracking URL, a negated term. Everything else enters the pool with a score.
//! The set is then classified once, and the evidence that can make it junk is:
//!
//! 1. its mean relevance is near zero, **and**
//! 2. it shares no rare term with the query.
//!
//! Shared rare terms protect a long topical answer from wholesale rejection.

use std::collections::HashSet;

use super::contract;
use super::engines::{AnswerClass, Hit};
use super::query::{QueryFacts, matches};

/// Retrieval confidence, not a claim that a page's assertions are true.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Confidence {
    High,
    Medium,
    #[default]
    Low,
}

/// Evidence behind a finished answer's confidence. Saved with cached answers.
#[derive(Debug, Clone, Copy, Default, serde::Serialize, serde::Deserialize)]
pub struct Assessment {
    pub confidence: Confidence,
    pub families: usize,
    pub agreement: usize,
    pub junk_ratio: f64,
    pub coverage: f64,
}

impl Assessment {
    pub fn degraded(self) -> bool {
        self.confidence == Confidence::Low || self.families < 2 || self.junk_ratio > 0.2
    }
}

/// Judge the returned evidence using the same independence graph as ranking.
/// Duplicate endpoints in a family never count as independent confirmation.
pub(crate) fn assess(
    query: &str,
    results: &[super::rank::Merged],
    reports: &[super::EngineReport],
    families: &super::families::Families,
) -> Assessment {
    let facts = QueryFacts::new(query);
    let independent: HashSet<_> = results
        .iter()
        .flat_map(|r| &r.sources)
        .map(|(source, _)| super::rank::family_name(families, source))
        .collect();
    let agreement = results
        .iter()
        .take(3)
        .map(|r| r.families)
        .max()
        .unwrap_or(0);
    // Unmeasured endpoints start as separate ranking clusters, but that is
    // not yet proof of independent agreement. High confidence needs a pair
    // actually observed to be distinct, or a first-party interface.
    let measured_agreement = results.iter().take(3).any(|result| {
        result.sources.iter().enumerate().any(|(i, (a, _))| {
            result.sources[i + 1..]
                .iter()
                .any(|(b, _)| super::independent_pair(a, b, families))
        })
    });
    let attempted = reports
        .iter()
        .filter(|r| matches!(r.status.as_str(), "ok" | "cached" | "thin" | "off-query"))
        .count();
    let junk = reports.iter().filter(|r| r.status == "off-query").count();
    let junk_ratio = junk as f64 / attempted.max(1) as f64;
    let coverage = results
        .iter()
        .take(3)
        .map(|r| facts.coverage(&format!("{} {} {}", r.title, r.snippet, r.url)))
        .sum::<f64>()
        / results.len().clamp(1, 3) as f64;
    let foreign = super::locale::evidence_language(query).is_some_and(|lang| {
        results
            .iter()
            .take(3)
            .any(|r| !super::locale::native(lang, &r.title, &r.snippet))
    });
    let confidence = if results.is_empty() || foreign || coverage < 0.15 || junk_ratio > 0.5 {
        Confidence::Low
    } else if independent.len() >= 2
        && measured_agreement
        && agreement >= 2
        && coverage >= 0.45
        && junk_ratio <= 0.2
    {
        Confidence::High
    } else {
        Confidence::Medium
    };
    Assessment {
        confidence,
        families: independent.len(),
        agreement,
        junk_ratio,
        coverage,
    }
}

/// Mean relevance below which a set with no shared rare term is answering a
/// different question. Calibrated against the receipt in `design/search.md` §1:
/// ten unrelated news items on a code query sit near zero; a set of real but
/// mediocre pages sits above 0.1.
const JUNK_MEAN: f64 = 0.06;

/// Below this a set is real but thin: the answer exists, the evidence is weak.
const THIN_MEAN: f64 = 0.22;

/// Fewer items than this is thin by construction: there is nothing to choose
/// from, whatever the scores say.
const THIN_ITEMS: usize = 3;

/// One answer, judged.
#[derive(Debug)]
pub struct Judgement {
    pub class: AnswerClass,
    /// The items that may enter the merge, in the order the source ranked them.
    pub kept: Vec<Hit>,
    /// What the source returned, before anything was vetoed.
    pub items: usize,
    /// Mean graded relevance of the kept items, 0..=1.
    pub mean: f64,
}

impl Judgement {
    fn empty(items: usize) -> Self {
        Self {
            class: AnswerClass::Empty,
            kept: Vec::new(),
            items,
            mean: 0.0,
        }
    }
}

/// Judge one source's answer.
pub fn judge(facts: &QueryFacts, hits: Vec<Hit>) -> Judgement {
    let items = hits.len();
    if items == 0 {
        return Judgement::empty(0);
    }
    // The publisher's own URL before anything else reads it: a wrapper is not a
    // candidate, it is a link to one.
    let hits: Vec<Hit> = hits
        .into_iter()
        .map(|mut h| {
            h.url = contract::publisher_url(&h.url);
            h
        })
        .collect();

    // The veto list, then duplicates. A duplicate is one page wearing two hats:
    // same canonical URL, or the same title with different tracking.
    let mut seen: HashSet<String> = HashSet::new();
    let mut kept: Vec<Hit> = Vec::with_capacity(items);
    for h in hits {
        // This is a **search engine's** answer, so it faces the full lane veto:
        // the universal list plus the two shape vetoes that were measured on
        // this very corpus — the encyclopedia or video a one-identifier query
        // must never be answered by (`tokio::spawn` → ja.wikipedia's article on
        // the band TOKIO), and the site's bare front door. A first-party API
        // never reaches this function ([`super::contract::accept_first_party`]
        // judges those), because its own ranking is its contract.
        if contract::veto_lane(facts, &h).is_some() {
            continue;
        }
        if !seen.insert(contract::canonical_url(&h.url)) {
            continue;
        }
        kept.push(h);
    }

    // A look-alike is only a look-alike when the answer set proves the name it
    // meant: if some scoped page's root is a query term, pages of the same shape
    // with another root are cousins, not answers. Without that anchor the query
    // is topical and its scoped results are left alone.
    kept = drop_look_alikes(facts, kept);

    // A query written in a non-Latin script prefers an answer written in it —
    // but only when one exists. Dropping the foreign items unconditionally is
    // how a Japanese query with English documentation ends up with nothing.
    kept = prefer_native(facts, kept);

    let mean = if kept.is_empty() {
        0.0
    } else {
        kept.iter()
            .map(|h| contract::relevance(facts, &h.title, &h.snippet, &h.url))
            .sum::<f64>()
            / kept.len() as f64
    };
    let rare_shared = has_rare_term(facts, &kept);

    // An answer in a different script cannot prove another topic by lexical
    // absence alone (東京の天気 -> Tokyo weather). Keep that fallback
    // thin; it has no evidence for high confidence or caching.
    let foreign = !kept.is_empty()
        && super::locale::evidence_language(&facts.compiled().text).is_some_and(|lang| {
            !kept
                .iter()
                .any(|h| super::locale::native(lang, &h.title, &h.snippet))
        });
    let class = if foreign {
        AnswerClass::Thin
    } else {
        classify(items, &kept, mean, rare_shared)
    };
    // A junk set contributes nothing, by construction: `kept` is exactly what may
    // enter the merge, so no caller can merge it by forgetting to check the
    // class. `items` still reports what the source returned.
    if class == AnswerClass::Junk {
        kept.clear();
    }
    Judgement {
        class,
        kept,
        items,
        mean,
    }
}

fn classify(items: usize, kept: &[Hit], mean: f64, rare_shared: bool) -> AnswerClass {
    if kept.is_empty() {
        // Every item was a wrapper, a SERP, a negation or a duplicate. That is
        // an answer about links, not an answer.
        return if items >= THIN_ITEMS {
            AnswerClass::Junk
        } else {
            AnswerClass::Thin
        };
    }
    // Provably another topic: no item shares a rare term with the query and the
    // set's mean relevance is near zero. §4 wrote this with a third condition —
    // "and the query names an entity" — to protect a long topical query from
    // being discarded wholesale. Measured on the 5.3.0 A corpus, that protection
    // is already carried by the rare-term test, and the entity requirement turns
    // out to *admit* the failure it was meant to prevent: `deno runtime`,
    // `地震 速報 緊急` and `人工知能 とは` name no capitalised entity, so a lane
    // serving its own furniture (shopping.yahoo.com/gift-ideas, youtube.com,
    // a summer-travel listicle) was kept and filled the list with it. A set that
    // shares nothing rare with the query is not a mediocre answer to a long
    // question; it is an answer to a different one.
    if mean < JUNK_MEAN && !rare_shared {
        return AnswerClass::Junk;
    }
    if items < THIN_ITEMS || kept.len() < THIN_ITEMS || mean < THIN_MEAN {
        return AnswerClass::Thin;
    }
    AnswerClass::Answered
}

fn has_rare_term(facts: &QueryFacts, hits: &[Hit]) -> bool {
    let rare = facts.rare_terms();
    // A query with no rare term (`rfc 9110`, `tokio`) gives this test no
    // evidence either way, so it must not be read as "shares nothing": that
    // dropped a real answer for `rfc 9110 filetype:pdf` (measured, paired
    // re-check 5.3.0 A). No rare term means "cannot prove another topic".
    if rare.is_empty() {
        return true;
    }
    hits.iter().any(|h| {
        let text = format!("{} {} {}", h.title, h.snippet, h.url);
        rare.iter().any(|t| matches(t, &text))
    })
}

fn drop_look_alikes(facts: &QueryFacts, hits: Vec<Hit>) -> Vec<Hit> {
    let mut named: std::collections::HashMap<contract::ScopeShape, HashSet<String>> =
        Default::default();
    for h in &hits {
        if let Some((shape, root)) = contract::scoped_root(&h.url)
            && facts.names(&root)
        {
            named.entry(shape).or_default().insert(root);
        }
    }
    if named.is_empty() {
        return hits;
    }
    hits.into_iter()
        .filter(|h| match contract::scoped_root(&h.url) {
            Some((shape, root)) => named.get(&shape).is_none_or(|roots| roots.contains(&root)),
            None => true,
        })
        .collect()
}

/// Keep the items written in the query's own script when the set has any,
/// otherwise keep them all: no native answer is not a reason to have none.
fn prefer_native(facts: &QueryFacts, hits: Vec<Hit>) -> Vec<Hit> {
    if facts.script() == super::query::Script::Latin {
        return hits;
    }
    let native = |h: &Hit| {
        let text = format!("{} {}", h.title, h.snippet);
        text.chars().any(|c| shares_script(facts.script(), c))
    };
    if !hits.iter().any(native) {
        return hits;
    }
    hits.into_iter().filter(native).collect()
}

fn shares_script(script: super::query::Script, c: char) -> bool {
    use super::query::Script as S;
    match script {
        S::Latin => c.is_ascii_alphabetic(),
        S::Kana => ('\u{3040}'..='\u{30ff}').contains(&c),
        S::Han => ('\u{4e00}'..='\u{9fff}').contains(&c) || ('\u{3400}'..='\u{4dbf}').contains(&c),
        S::Hangul => ('\u{ac00}'..='\u{d7af}').contains(&c),
        S::Cyrillic => ('\u{0400}'..='\u{04ff}').contains(&c),
        S::Arabic => ('\u{0600}'..='\u{06ff}').contains(&c),
        S::Devanagari => ('\u{0900}'..='\u{097f}').contains(&c),
        S::Other => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hit(title: &str, url: &str) -> Hit {
        Hit {
            title: title.into(),
            url: url.into(),
            snippet: String::new(),
            rank: 1,
            published: None,
        }
    }

    fn hits(rows: &[(&str, &str)]) -> Vec<Hit> {
        rows.iter().map(|(t, u)| hit(t, u)).collect()
    }

    #[test]
    fn confidence_requires_independent_agreement_and_query_evidence() {
        use super::super::rank::Merged;
        let mut result = Merged {
            title: "Tokio runtime".into(),
            url: "https://tokio.rs/guide".into(),
            snippet: "Tokio runtime guide".into(),
            sources: vec![("bing".into(), 1), ("yahoo".into(), 1)],
            score: 1.0,
            published: None,
            families: 1,
            from_page: false,
            tier: 0,
        };
        let mut families = super::super::families::Families::default();
        let urls: Vec<String> = (0..5).map(|n| format!("https://tokio.rs/{n}")).collect();
        for _ in 0..3 {
            families.observe(&[
                ("bing".into(), urls.clone()),
                ("yahoo".into(), urls.clone()),
            ]);
        }
        let grade = assess("tokio runtime", &[result.clone()], &[], &families);
        assert_ne!(grade.confidence, Confidence::High);
        assert!(grade.degraded());
        result.sources.push(("brave".into(), 1));
        result.families = 2;
        let grade = assess("tokio runtime", &[result.clone()], &[], &families);
        assert_eq!(
            grade.confidence,
            Confidence::Medium,
            "an unmeasured endpoint is not independent evidence yet"
        );
        let other: Vec<String> = (40..45).map(|n| format!("https://tokio.rs/{n}")).collect();
        for _ in 0..3 {
            families.observe(&[
                ("bing".into(), urls.clone()),
                ("brave".into(), other.clone()),
            ]);
        }
        let grade = assess("tokio runtime", &[result.clone()], &[], &families);
        assert_eq!(grade.confidence, Confidence::High);
        assert!(!grade.degraded());
        let foreign = assess("tokio runtime 日本語", &[result.clone()], &[], &families);
        assert_eq!(foreign.confidence, Confidence::Low);
        assert!(foreign.degraded());
        let report = |status: &str| super::super::EngineReport {
            engine: "test".into(),
            profile: None,
            status: status.into(),
            hits: 1,
            ms: 0,
            egress: "direct".into(),
            requests: 0,
            cache_age_ms: None,
        };
        let reports = [report("cached"), report("off-query")];
        assert_eq!(
            assess("tokio runtime", &[result.clone()], &reports, &families).junk_ratio,
            0.5
        );
        let empty = assess("tokio runtime", &[], &[], &families);
        assert_eq!(empty.confidence, Confidence::Low);
        result.title = "Holiday gift guide".into();
        result.snippet = "Winter shopping".into();
        result.url = "https://example.com/gifts".into();
        assert_eq!(
            assess("tokio runtime", &[result], &[], &families).confidence,
            Confidence::Low
        );
    }

    #[test]
    fn a_query_with_no_rare_term_cannot_prove_another_topic() {
        // `rfc 9110` has no term long enough to be rare evidence. A set that
        // shares nothing must not be read as junk: there is nothing to share.
        let facts = QueryFacts::new("rfc 9110");
        assert!(facts.rare_terms().is_empty());
        let rows = hits(&[
            ("RFC 9110", "https://www.rfc-editor.org/rfc/rfc9110.html"),
            ("RFC 9110", "https://datatracker.ietf.org/doc/rfc9110/"),
            ("RFC 9110", "https://www.rfc-editor.org/rfc/rfc9110.txt"),
        ]);
        let j = judge(&facts, rows);
        assert_ne!(j.class, AnswerClass::Junk);
        assert_eq!(j.kept.len(), 3);
    }

    #[test]
    fn a_lane_serving_its_own_furniture_is_junk_without_a_named_entity() {
        // Measured in the A gate run: for `deno runtime` — no capitalised entity
        // to anchor on — yahoo answered with its own shopping and finance front
        // pages, and the set filled the list.
        let facts = QueryFacts::new("deno runtime");
        assert!(facts.anchors().is_empty());
        let rows = hits(&[
            ("Gift Ideas", "https://shopping.yahoo.com/gift-ideas/"),
            ("Portfolios", "https://finance.yahoo.com/portfolios/"),
            ("Guides", "https://shopping.yahoo.com/guides/"),
        ]);
        let j = judge(&facts, rows);
        assert_eq!(j.class, AnswerClass::Junk);
        assert!(j.kept.is_empty());
    }

    #[test]
    fn a_long_topical_query_is_never_discarded_wholesale() {
        // The receipt from design/search.md §1: thirty real results, dropped by a
        // coverage guillotine, on a query that names no entity. Low per-item
        // coverage is expected on a long question and is not a verdict.
        let facts =
            QueryFacts::new("fastembed rust model2vec static embeddings CPU small fast rerank");
        let rows: Vec<Hit> = (0..10)
            .map(|i| {
                hit(
                    &format!("fastembed {i}"),
                    &format!("https://example.com/model2vec/{i}"),
                )
            })
            .collect();
        let j = judge(&facts, rows);
        assert_ne!(j.class, AnswerClass::Junk, "mean {:.3}", j.mean);
        assert_eq!(j.kept.len(), 10, "nothing is vetoed for weak coverage");
    }

    #[test]
    fn a_covered_long_answer_is_answered_not_thin() {
        let facts =
            QueryFacts::new("fastembed rust model2vec static embeddings CPU small fast rerank");
        let rows: Vec<Hit> = (0..5)
            .map(|i| {
                hit(
                    "fastembed: fast CPU embeddings for Rust — model2vec static embeddings",
                    &format!("https://example.com/model2vec/{i}"),
                )
            })
            .collect();
        let j = judge(&facts, rows);
        assert_eq!(j.class, AnswerClass::Answered, "mean {:.3}", j.mean);
    }

    #[test]
    fn a_set_that_shares_no_rare_term_with_a_named_entity_is_junk() {
        let facts = QueryFacts::new("OpenCode release notes");
        let rows = hits(&[
            (
                "silverbulletmd/silverbullet",
                "https://github.com/silverbulletmd/silverbullet",
            ),
            (
                "microsoft/markitdown",
                "https://github.com/microsoft/markitdown",
            ),
            (
                "charmbracelet/crush",
                "https://github.com/charmbracelet/crush",
            ),
        ]);
        let j = judge(&facts, rows);
        assert_eq!(j.class, AnswerClass::Junk);
        assert!(j.kept.is_empty());
    }

    #[test]
    fn a_short_wrong_topic_is_junk_and_a_short_real_answer_is_thin() {
        let facts = QueryFacts::new("OpenCode release notes");
        let rows = hits(&[(
            "charmbracelet/crush",
            "https://github.com/charmbracelet/crush",
        )]);
        assert_eq!(judge(&facts, rows).class, AnswerClass::Junk);
        let real = hits(&[("OpenCode release notes", "https://opencode.ai/changelog")]);
        assert_eq!(judge(&facts, real).class, AnswerClass::Thin);
    }

    #[test]
    fn a_wrapper_or_a_serp_never_becomes_a_candidate() {
        let facts = QueryFacts::new("tokio select");
        let rows = hits(&[
            ("a", "https://news.google.com/rss/articles/CBMi"),
            ("b", "https://www.bing.com/search?q=tokio"),
            (
                "tokio::select",
                "https://docs.rs/tokio/latest/tokio/macro.select.html",
            ),
        ]);
        let j = judge(&facts, rows);
        assert_eq!(j.kept.len(), 1);
        assert_eq!(j.items - j.kept.len(), 2);
        assert_eq!(j.class, AnswerClass::Thin, "one real item is thin");
    }

    #[test]
    fn the_same_page_arriving_twice_is_one_candidate() {
        let facts = QueryFacts::new("tokio select");
        let rows = hits(&[
            (
                "tokio::select",
                "https://docs.rs/tokio/latest/tokio/macro.select.html?utm_source=x",
            ),
            (
                "select!",
                "https://docs.rs/tokio/latest/tokio/macro.select.html",
            ),
            ("select", "https://tokio.rs/tokio/tutorial/select"),
        ]);
        let j = judge(&facts, rows);
        assert_eq!(
            j.kept.len(),
            2,
            "{:?}",
            j.kept.iter().map(|h| &h.url).collect::<Vec<_>>()
        );
    }

    #[test]
    fn a_negated_term_vetoes_the_hit() {
        let facts = QueryFacts::new("tokio tutorial -youtube");
        let rows = hits(&[
            ("Tokio tutorial", "https://tokio.rs/tokio/tutorial"),
            ("Tokio tutorial video", "https://www.youtube.com/watch?v=1"),
        ]);
        let j = judge(&facts, rows);
        assert_eq!(j.kept.len(), 1);
        assert_eq!(j.items - j.kept.len(), 1);
    }

    #[test]
    fn a_native_answer_wins_when_the_query_is_written_in_its_script() {
        let facts = QueryFacts::new("東京の天気");
        let rows = hits(&[
            ("Tokyo weather", "https://example.com/en/tokyo-weather"),
            (
                "東京の天気予報",
                "https://weathernews.jp/onebox/tenki/tokyo/",
            ),
        ]);
        let j = judge(&facts, rows);
        assert_eq!(j.kept.len(), 1);
        assert!(j.kept[0].title.contains("東京"));
    }

    #[test]
    fn a_foreign_answer_survives_when_there_is_no_native_one() {
        let facts = QueryFacts::new("東京の天気");
        let rows = hits(&[("Tokyo weather", "https://example.com/en/tokyo-weather")]);
        let j = judge(&facts, rows);
        assert_eq!(
            j.kept.len(),
            1,
            "no native answer is not a reason to have none"
        );
    }

    #[test]
    fn a_look_alike_namespace_is_dropped_only_when_the_set_proves_the_name() {
        let facts = QueryFacts::new("tokio spawn_blocking");
        let rows = hits(&[
            (
                "tokio::task::spawn_blocking",
                "https://docs.rs/tokio/latest/tokio/task/fn.spawn_blocking.html",
            ),
            (
                "other::task::spawn_blocking",
                "https://docs.rs/other/latest/other/task/fn.spawn_blocking.html",
            ),
        ]);
        let j = judge(&facts, rows.clone());
        assert_eq!(j.kept.len(), 1, "the other crate is a cousin");
        // A topical query has no named root: both stay.
        let topical = QueryFacts::new("spawn blocking thread pool");
        let j = judge(&topical, rows);
        assert_eq!(j.kept.len(), 2);
    }
}
