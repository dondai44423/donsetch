//! rank.rs : weighted RRF + consensus + relevance +
//! priors + diversity. This is where naive-merge
//! metasearch (SearXNG) loses.

use std::collections::HashMap;

use super::engines::Hit;
use super::families::Families;
use super::intent::Intent;

/// How fast a result's mass falls with rank. Measured insensitive: k from 5 to
/// 300 moved hit@1 by less than one query on the 40-query set, so the standard
/// value stays.
const RRF_K: f64 = 60.0;
/// Each independent family beyond the first that returns a URL lifts it by this
/// fraction. Agreement between different indexes is the one signal here that
/// text matching cannot fake.
const CONSENSUS_MULT: f64 = 0.5;
const MAX_PER_DOMAIN: usize = 2;
/// Vertical-only hits rank below web-engine consensus:
/// a GitHub/HN/wiki hit with no engine corroboration is
/// a weaker signal than a URL three engines agree on.
const VERTICAL_WEIGHT: f64 = 0.6;

pub(crate) fn is_vertical(engine: &str) -> bool {
    matches!(
        engine,
        "github"
            | "hn"
            | "wikipedia"
            | "scholar"
            | "news"
            | "arxiv"
            | "stackexchange"
            | "mdn"
            | "openalex"
            | "europepmc"
            | "crates"
            | "npm"
            | "registry"
            | "registry-any"
            | "bing-news"
            | "feed"
            | "gdelt"
    )
}

/// A merged, scored result.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Merged {
    pub title: String,
    pub url: String,
    pub snippet: String,
    /// (engine, rank) pairs that produced this result.
    pub sources: Vec<(String, usize)>,
    pub score: f64,
    /// News vertical fills this; freshness ranking reads it.
    pub published: Option<String>,
    /// Independent families that returned this URL, as measured when it was
    /// ranked (`families.rs`). Cached results keep the count they were given.
    #[serde(default)]
    pub families: usize,
    /// The snippet is a passage read from the page itself (`read`), not a
    /// source's teaser.
    #[serde(default)]
    pub from_page: bool,
    /// Admission class: 0 the resolver's own record for the name, 1 everything
    /// else. Only a registry answer is proof that the query named the page; a
    /// scoped URL an engine happened to find is not (an npm package called
    /// "photosynthesis" is not the encyclopedia article). Inside a class the
    /// measured score decides.
    #[serde(default)]
    pub tier: u8,
}

/// Normalize a URL for consensus matching: scheme-less,
/// www-less, trailing-slash-less, lowercase host.
pub fn norm_key(raw: &str) -> String {
    let Ok(u) = url::Url::parse(raw) else {
        return raw.to_lowercase();
    };
    let host = u
        .host_str()
        .unwrap_or("")
        .trim_start_matches("www.")
        .to_lowercase();
    // An arXiv paper is one document at /abs, /pdf and /html alike.
    let arxiv_id = (host == "arxiv.org")
        .then(|| {
            let p = u.path();
            ["/abs/", "/pdf/", "/html/"]
                .iter()
                .find_map(|pre| p.strip_prefix(pre))
                .map(|id| id.trim_end_matches(".pdf").trim_end_matches('/'))
        })
        .flatten()
        .map(|id| format!("/abs/{id}"));
    let path = arxiv_id.unwrap_or_else(|| u.path().to_string());
    let path = path
        .trim_end_matches('/')
        .trim_end_matches("/index.html")
        .trim_end_matches("/index.htm")
        .to_lowercase();
    let query = u
        .query()
        .map(|query| {
            let kept = query
                .split('&')
                .filter(|pair| {
                    url::form_urlencoded::parse(pair.as_bytes())
                        .next()
                        .is_none_or(|(key, _)| !is_tracking_query_key(&key))
                })
                .collect::<Vec<_>>();
            if kept.is_empty() {
                String::new()
            } else {
                format!("?{}", kept.join("&"))
            }
        })
        .unwrap_or_default();
    format!("{host}{path}{query}")
}

fn is_tracking_query_key(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    key.starts_with("utm_")
        || matches!(
            key.as_str(),
            "fbclid"
                | "gclid"
                | "dclid"
                | "msclkid"
                | "msockid"
                | "mc_cid"
                | "mc_eid"
                | "igshid"
                | "ref_src"
                | "_ga"
                | "_gl"
        )
}

pub fn host_of(raw: &str) -> String {
    url::Url::parse(raw)
        .ok()
        .and_then(|u| u.host_str().map(String::from))
        .unwrap_or_default()
        .trim_start_matches("www.")
        .to_string()
}

/// Collapse every whitespace run : newlines included : into a
/// single space. HTML-scraped engines already do this in
/// `engines::text`, but JSON-sourced hits arrive raw: MDN
/// summaries, BYOK provider snippets (Exa returns page text),
/// GitHub descriptions. A newline inside a snippet breaks the
/// three-space indent of the markdown list, so normalize once
/// here : the one point every source flows through : rather
/// than per parser, where the next engine would miss it.
fn collapse_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Every source's title passes through the merge once; a provider
/// body is bounded only by the 64 MiB fetch cap, and the plugin
/// adapter already cuts its titles at this length (#266). Cut here
/// so the rendered line and structuredContent cannot carry a
/// multi-MiB title from any provider.
const MAX_TITLE_CHARS: usize = 512;

/// How many candidates the cross-encoder scores. 24 is past the point where a
/// tie-break can matter: the answer is in the head of the list or it is not.
const RERANK_DEPTH: usize = 24;

/// No snippet may exceed this, whatever the source. The render cuts to 180 and
/// the cross-encoder batches every candidate to the longest snippet it is given,
/// so one unbounded description costs seconds of CPU on every search.
const MAX_SNIPPET_CHARS: usize = 600;

fn title_of(s: &str) -> String {
    collapse_ws(s).chars().take(MAX_TITLE_CHARS).collect()
}

/// Merge all engine hits into one ranked list.
/// `trust` maps engine -> 0.0..=2.0 (learned EWMA).
pub fn merge(
    per_engine: &[(String, Vec<Hit>)],
    query: &str,
    intent: Intent,
    trust: &HashMap<String, f64>,
    families: &Families,
    max_results: usize,
) -> Vec<Merged> {
    merge_inner(
        per_engine,
        query,
        intent,
        trust,
        families,
        max_results,
        true,
    )
}

/// Complete consensus/lexical ranking when the semantic worker exceeds the caller budget.
#[cfg(feature = "rerank")]
pub(crate) fn merge_lexical(
    per_engine: &[(String, Vec<Hit>)],
    query: &str,
    intent: Intent,
    trust: &HashMap<String, f64>,
    families: &Families,
    max_results: usize,
) -> Vec<Merged> {
    merge_inner(
        per_engine,
        query,
        intent,
        trust,
        families,
        max_results,
        false,
    )
}

/// Fusion over measured independence, nothing else.
///
/// A URL's score is the best rank-decayed vote of each independent family that
/// returned it, lifted a little for every extra family. There is no text
/// scoring of titles and snippets and no domain table. Measured on a 40-query
/// gold set with the sources' own results held fixed, BM25 over titles, a domain
/// prior and an authority multiplier took hit@1 from 0.62 down to 0.17: the
/// additive terms were ten to twenty times the size of the rank signal and
/// promoted exactly the pages a search engine had already learned to bury (SEO
/// restatements of the query). What survives is a bounded coverage penalty for
/// wrong versions and mangled compound terms, and a freshness lift for dated
/// news items.
fn merge_inner(
    per_engine: &[(String, Vec<Hit>)],
    query: &str,
    intent: Intent,
    trust: &HashMap<String, f64>,
    families: &Families,
    max_results: usize,
    semantic: bool,
) -> Vec<Merged> {
    // Group by normalized URL. RRF mass is counted PER MEASURED FAMILY, not per
    // engine: sources that return the same results are one opinion, so a farm
    // ranked by all of them out-masses nobody (bench round 3: three engines on
    // one index beat full-weight Wikipedia on conceptual queries). Which sources
    // are one family is learned from their overlap (`families.rs`), never named.
    let mut groups: HashMap<String, Merged> = HashMap::new();
    // First-seen order of the URL keys. A `HashMap` iterates in a per-process
    // random order, so any two results with an equal score came out in an
    // arbitrary order: the same candidates, ranked differently on each run.
    let mut order: Vec<String> = Vec::new();
    // Every source's snippet for a URL: the one shown is picked from these once
    // the order is settled (`evidence::pick`), so ranking never reads a cut text.
    let mut snippets: HashMap<String, Vec<String>> = HashMap::new();
    let mut family_best: HashMap<(String, String), f64> = HashMap::new();
    let conceptual = super::intent::is_conceptual(query);
    // Sources are read in a fixed order (trust, then name), not the order they
    // happened to answer in: a tie then resolves to the same URL on every run.
    let mut sources: Vec<&(String, Vec<Hit>)> = per_engine.iter().collect();
    sources.sort_by(|a, b| {
        let t = |e: &str| {
            trust
                .get(super::egress::health_key(e))
                .copied()
                .unwrap_or(1.0)
        };
        t(&b.0).total_cmp(&t(&a.0)).then_with(|| a.0.cmp(&b.0))
    });
    for (engine, hits) in sources {
        // Learned trust is keyed by the engine's HEALTH key: the
        // ddg_html retry lane books its health under "ddg", so a
        // lookup by the lane label silently fell back to default
        // trust and discarded everything the loop had learned.
        let base = trust
            .get(super::egress::health_key(engine))
            .copied()
            .unwrap_or(1.0);
        // Wikipedia on a conceptual query is not a "vertical
        // hint" : it IS the canonical encyclopedia engine.
        // Full weight keeps farm consensus from outranking
        // the explainer humans actually trust.
        let authoritative = conceptual && engine == "wikipedia";
        let tw = if is_vertical(engine) && !authoritative {
            base * VERTICAL_WEIGHT
        } else {
            base
        };
        // Verticals are independent sources : each is its
        // OWN family (deduping arxiv against scholar or
        // github against hn would destroy their mass).
        let family = family_name(families, engine);
        for hit in hits {
            // Normalized once per hit: every comparison below is
            // length-based, so mixing raw and collapsed strings
            // would pick winners by whitespace count.
            let hit_title = title_of(&hit.title);
            // Bounded once, here, where every source's text enters the merge.
            let hit_snippet: String = collapse_ws(&hit.snippet)
                .chars()
                .take(MAX_SNIPPET_CHARS)
                .collect();
            let key = norm_key(&hit.url);
            if !hit_snippet.is_empty() {
                let seen = snippets.entry(key.clone()).or_default();
                if !seen.contains(&hit_snippet) {
                    seen.push(hit_snippet.clone());
                }
            }
            let contribution = tw / (RRF_K + hit.rank as f64 + 1.0);
            let best = family_best
                .entry((key.clone(), family.clone()))
                .or_insert(0.0);
            *best = best.max(contribution);
            if !groups.contains_key(&key) {
                order.push(key.clone());
            }
            let entry = groups.entry(key).or_insert_with(|| Merged {
                title: hit_title.clone(),
                url: hit.url.clone(),
                snippet: hit_snippet.clone(),
                sources: Vec::new(),
                score: 0.0,
                published: None,
                families: 0,
                from_page: false,
                tier: 1,
            });
            // Keep the longest snippet (most informative), skipping redirect
            // stubs. Both sides are already bounded (MAX_SNIPPET_CHARS), so no
            // source can inflate the cross-encoder's batch.
            if hit_snippet.len() > entry.snippet.len() && !hit_snippet.starts_with("Redirecting") {
                entry.snippet = hit_snippet.clone();
            }
            // Best title: breadcrumbs ("a › b › c") and
            // URL-echoes are longer than real titles : keep
            // the shortest CLEAN candidate.
            let bad = |t: &str| t.contains(" › ") || t.starts_with("http") || t.len() < 3;
            if !bad(&hit_title) && (bad(&entry.title) || hit_title.len() < entry.title.len()) {
                entry.title = hit_title.clone();
            }
            if entry.published.is_none() && hit.published.is_some() {
                entry.published = hit.published.clone();
            }
            entry.sources.push((engine.clone(), hit.rank));
        }
    }
    // Sum the per-family best contributions.
    for ((key, _family), contribution) in family_best {
        if let Some(entry) = groups.get_mut(&key) {
            entry.score += contribution;
        }
    }

    let mut results: Vec<Merged> = order.iter().filter_map(|key| groups.remove(key)).collect();

    // Consensus multiplier: one step for every independent family beyond the
    // first that returned the URL.
    for r in &mut results {
        let independent = r
            .sources
            .iter()
            .map(|(e, _)| family_name(families, e))
            .collect::<std::collections::HashSet<_>>()
            .len();
        r.families = independent;
        r.score *= 1.0 + CONSENSUS_MULT * (independent as f64 - 1.0).max(0.0);
    }

    // Vertical-only penalty: a result from ONLY a vertical
    // (github, hn, wikipedia) with no general web engine
    // corroboration is a weaker signal. The vertical found it
    // by keyword match; general engines not surfacing it means
    // it's probably tangential. Applied after BM25+prior so
    // it affects the pre-rerank score that feeds the rerank
    // blend : the 60% RRF weight keeps it below consensus
    // results even when the cross-encoder scores it high.
    // The `authoritative` flag already gives full RRF weight
    // (no VERTICAL_WEIGHT reduction) : that's enough of a
    // boost. The penalty for zero general-engine corroboration
    // applies equally: if no SERP found the Wikipedia article,
    // it's probably the wrong one (e.g. Linearizability for
    // a CRDT query).
    for r in &mut results {
        let has_general = r.sources.iter().any(|(e, _)| !is_vertical(e));
        if !has_general {
            r.score *= 0.4;
        }
    }

    // Cross-encoder semantic reranking: re-score by semantic
    // relevance (query ↔ title+snippet through full attention).
    // Skipped gracefully if model unavailable or feature disabled.
    if semantic && crate::config::cfg().search.rerank {
        // The cross-encoder is a tie-breaker over the strongest candidates, not
        // a second retrieval: score the head of the lexical blend and leave the
        // tail alone, so its cost is bounded by a constant, not by the merge.
        results.sort_by(|a, b| b.score.total_cmp(&a.score));
        let depth = results.len().min(RERANK_DEPTH);
        crate::search::rerank::rerank(query, &mut results[..depth]);
    }

    // Entity coverage penalty: penalize results that miss
    // key query entities (compound terms like "B-tree", wrong
    // version numbers like "5.5" when the query says "5.2").
    // Sits after rerank so the cross-encoder can still rescue
    // semantically-relevant results, but coverage gaps can't
    // be fully overridden : "binary tree" ≠ "B-tree" no matter
    // how similar the cross-encoder thinks they are.
    crate::search::coverage::penalize(query, &mut results);

    // Dated news items rank fresh ones up and stale ones down; nothing else
    // about a result's host or title moves it.
    crate::search::authority::freshen(intent, &mut results);

    // Admission classes, not score multipliers. Two things are proof enough to
    // lead: the resolver's own record, and (for a code query) a package
    // registry page whose root the query names and whose text covers the query.
    // Everything else keeps the measured score. D1 promoted any scoped URL and
    // measured the cost: an npm package called "photosynthesis" took rank 1
    // from the encyclopedia, a GitHub changelog from a release note, and a
    // crates page matched on one word from the crate the query actually named.
    let profile = super::contract::Profile::new(query);
    for r in &mut results {
        let resolved = r.sources.iter().any(|(engine, _)| engine == "registry");
        let record = intent == Intent::Code
            && super::contract::package_scope_named(&profile, &r.url)
            && profile.verdict(&r.title, &r.snippet, &r.url) == super::contract::Verdict::Exact;
        // A dated item from the news program answers a dated query: one record
        // per news lane leads (its top item), not the whole wire — promoting
        // every wire item buried the official page an announcement points at.
        let dated_news = r.published.is_some()
            && r.sources.iter().any(|(engine, rank)| {
                *rank == 0 && matches!(engine.as_str(), "bing-news" | "feed" | "gdelt")
            })
            && (intent == Intent::News || super::contract::window_for(query).is_some());
        r.tier = if resolved || record || dated_news {
            0
        } else {
            1
        };
    }

    results.sort_by(|a, b| {
        a.tier
            .cmp(&b.tier)
            .then_with(|| b.score.total_cmp(&a.score))
    });

    // Syndicated content dedup: same article republished under
    // different local TV/radio sites (fox43, localmemphis, etc.)
    // appears as separate hits with identical titles. Collapse
    // them: keep the first (highest-scoring), drop the rest.
    let mut deduped = Vec::with_capacity(results.len());
    let mut seen_titles: std::collections::HashSet<String> = std::collections::HashSet::new();
    for r in results {
        let title_key = syndication_title_key(&r.title);
        if !title_key.is_empty() && !seen_titles.insert(title_key) {
            continue; // duplicate title : syndicated content
        }
        deduped.push(r);
    }
    results = deduped;

    // Diversity cap: max MAX_PER_DOMAIN per domain.
    let mut domain_count: HashMap<String, usize> = HashMap::new();
    let mut diverse = Vec::with_capacity(max_results);
    let mut overflow = Vec::new();
    for r in results {
        let host = host_of(&r.url);
        let c = domain_count.entry(host).or_insert(0);
        if *c < MAX_PER_DOMAIN {
            *c += 1;
            diverse.push(r);
        } else {
            overflow.push(r);
        }
        if diverse.len() >= max_results {
            break;
        }
    }
    if diverse.len() < max_results {
        diverse.extend(overflow.into_iter().take(max_results - diverse.len()));
    }
    diverse.truncate(max_results);
    for r in &mut diverse {
        if let Some(candidates) = snippets.get(&norm_key(&r.url)) {
            r.snippet = super::evidence::pick(query, candidates, super::evidence::EVIDENCE_CHARS);
        }
    }
    diverse
}

/// Title key for the syndication dedup: lowercase with collapsed
/// whitespace, PUNCTUATION PRESERVED. The previous key stripped every
/// non-alphanumeric char, which collapsed genuinely different titles
/// on different domains — "C++ Tutorial", "C# Tutorial" and "C
/// Tutorial" all became "c tutorial" (and "Rust 1.75" == "Rust 175"),
/// so the lower-scoring distinct-language result was silently dropped
/// as "syndicated content." That is a common Code-intent recall loss.
/// Keeping punctuation means only titles that are genuinely identical
/// after case/whitespace normalization — the syndicated-republish
/// case the dedup targets — still collapse.
fn syndication_title_key(title: &str) -> String {
    title
        .to_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// The family a source's votes are counted under. First-party interfaces are
/// each their own: deduping arXiv against Semantic Scholar, or GitHub against
/// Hacker News, would erase the independence that makes them worth asking.
pub(crate) fn family_name(families: &Families, engine: &str) -> String {
    if is_vertical(engine) {
        format!("vertical:{engine}")
    } else {
        families.family_of(super::egress::health_key(engine))
    }
}

/// Independent families behind one merged result: the agreement evidence the
/// ranking itself uses, exposed so the model surface can carry it cheaply.
pub(crate) fn family_count(r: &Merged) -> usize {
    r.families.max(1)
}

/// Host category for fetch routing; it does not verify a page's claims.
pub fn source_type(raw: &str) -> &'static str {
    let host = host_of(raw);
    let host = host.trim_start_matches("www.");
    match host {
        "youtube.com" | "music.youtube.com" | "m.youtube.com" | "youtu.be" | "vimeo.com" => "video",
        "reddit.com" | "stackoverflow.com" | "news.ycombinator.com" | "github.com" => "community",
        "arxiv.org" | "aclanthology.org" | "openreview.net" => "paper",
        "rfc-editor.org" | "en.wikipedia.org" | "britannica.com" => "reference",
        "wsj.com" | "reuters.com" | "apnews.com" | "bbc.com" | "nytimes.com" | "techcrunch.com" => {
            "publisher"
        }
        "openai.com"
        | "anthropic.com"
        | "microsoft.com"
        | "developer.mozilla.org"
        | "docs.python.org"
        | "rust-lang.org" => "official",
        _ if host.ends_with(".gov") || host.ends_with(".edu") => "institutional",
        _ => "web",
    }
}

/// Total results before truncation : the cache and retry gates read it.
pub fn merged_total(per_engine: &[(String, Vec<super::engines::Hit>)]) -> usize {
    let mut keys = std::collections::HashSet::new();
    for (_, hits) in per_engine {
        for h in hits {
            keys.insert(norm_key(&h.url));
        }
    }
    keys.len()
}

#[cfg(test)]
mod tests {
    #[test]
    fn report_audit_source_types_do_not_trust_lookalike_domains() {
        assert_eq!(source_type("https://www.youtube.com/watch?v=abc"), "video");
        assert_eq!(source_type("https://openai.com/index/example/"), "official");
        assert_eq!(source_type("https://tech-insider.org/article/"), "web");
        assert_eq!(source_type("https://openai.com.evil.example/"), "web");
    }

    use super::*;
    use crate::search::engines::Hit;
    use crate::search::intent::Intent;

    fn hit(url: &str, rank: usize) -> Hit {
        Hit {
            title: format!("title for {url}"),
            url: url.into(),
            snippet: "rust async runtime comparison".into(),
            rank,
            published: None,
        }
    }

    // The plugin adapter cuts its titles (#266); the nine native
    // providers and the HTML scrapers did not, and this is the one
    // point every source passes.
    #[test]
    fn a_provider_title_is_cut_at_the_merge() {
        let mut h = hit("https://ex.com/a", 0);
        h.title = "t".repeat(20_000);
        let per = vec![("serper".to_string(), vec![h])];
        let trust = HashMap::new();
        let out = merge(&per, "q", Intent::Web, &trust, &Families::default(), 10);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].title.chars().count(), MAX_TITLE_CHARS);
    }

    #[test]
    fn only_the_registrys_own_record_leads_the_list() {
        // An npm package called "photosynthesis" is a real record, but it is
        // not the encyclopedia article the query asks for; a scoped URL found
        // by an engine is not proof of anything.
        let per = vec![
            (
                "ddg".to_string(),
                vec![hit("https://en.wikipedia.org/wiki/Photosynthesis", 1)],
            ),
            (
                "registry-any".to_string(),
                vec![hit("https://www.npmjs.com/package/photosynthesis", 1)],
            ),
        ];
        let out = merge(
            &per,
            "photosynthesis",
            Intent::Entity,
            &HashMap::new(),
            &Families::default(),
            10,
        );
        assert_eq!(out[0].url, "https://en.wikipedia.org/wiki/Photosynthesis");
        assert_eq!(out[1].tier, 1, "a bare-name guess is not a tier promotion");
    }

    #[test]
    fn a_code_query_leads_with_the_package_it_names() {
        let mut docs = hit("https://docs.rs/clap/latest/clap/", 8);
        docs.title = "clap — Rust crate documentation".into();
        docs.snippet =
            "A simple to use, efficient, and full-featured Command Line Argument Parser".into();
        let per = vec![
            (
                "ddg".to_string(),
                vec![hit("https://doc.rust-lang.org/book/ch12-01.html", 1)],
            ),
            ("brave".to_string(), vec![docs]),
        ];
        let out = merge(
            &per,
            "rust clap command line argument parser crate",
            Intent::Code,
            &HashMap::new(),
            &Families::default(),
            10,
        );
        assert_eq!(out[0].url, "https://docs.rs/clap/latest/clap/");
        assert_eq!(out[0].tier, 0);
    }

    #[test]
    fn a_record_matched_on_one_word_does_not_lead() {
        // The serde query is about serde_json; crates.io/crates/serde matches
        // one word of it and is not the record asked for.
        let mut serde = hit("https://crates.io/crates/serde", 1);
        serde.title = "serde".into();
        serde.snippet = "A serialization framework".into();
        let mut answer = hit("https://serde.rs/field-attrs.html", 1);
        answer.title = "Field attributes · Serde".into();
        answer.snippet = "Rename fields with serde json attributes".into();
        let per = vec![
            ("ddg".to_string(), vec![answer.clone()]),
            ("yahoo".to_string(), vec![answer]),
            ("brave".to_string(), vec![serde]),
        ];
        let out = merge(
            &per,
            "rust serde json crate",
            Intent::Code,
            &HashMap::new(),
            &Families::default(),
            10,
        );
        assert_eq!(out[0].url, "https://serde.rs/field-attrs.html");
        let crates = out
            .iter()
            .find(|r| r.url.contains("crates.io/crates/serde"))
            .expect("the crate page is still a result");
        assert_eq!(crates.tier, 1, "a one-word match is not a promotion");
    }

    #[test]
    fn a_resolved_record_leads_the_list() {
        let per = vec![
            (
                "ddg".to_string(),
                vec![hit("https://example.com/blog/tokio", 1)],
            ),
            (
                "registry".to_string(),
                vec![hit("https://docs.rs/tokio/latest/tokio/", 1)],
            ),
        ];
        let out = merge(
            &per,
            "rust tokio",
            Intent::Code,
            &HashMap::new(),
            &Families::default(),
            10,
        );
        assert_eq!(out[0].url, "https://docs.rs/tokio/latest/tokio/");
        assert_eq!(out[0].tier, 0);
    }

    #[test]
    fn consensus_beats_vertical_only() {
        // URL A: brave #5 + bing #5 (two families).
        // URL B: github vertical #0 only.
        let per = vec![
            ("brave".to_string(), vec![hit("https://a.com/x", 5)]),
            ("bing".to_string(), vec![hit("https://a.com/x", 5)]),
            ("github".to_string(), vec![hit("https://b.com/y", 0)]),
        ];
        let trust = std::collections::HashMap::new();
        let out = merge(
            &per,
            "rust async runtime",
            Intent::Code,
            &trust,
            &Families::default(),
            10,
        );
        assert_eq!(out[0].url, "https://a.com/x", "consensus must win");
    }

    // The syndication dedup keyed titles after stripping ALL
    // punctuation, so "C++ Tutorial" and "C# Tutorial" on different
    // domains collapsed to one key and the lower-scoring one was
    // dropped as syndicated content. Distinct-language / versioned
    // results must both survive; only genuinely identical titles
    // (the republish case) collapse.
    #[test]
    fn syndication_dedup_keeps_distinct_punctuation_titles() {
        assert_ne!(
            syndication_title_key("C++ Tutorial"),
            syndication_title_key("C# Tutorial")
        );
        assert_ne!(
            syndication_title_key("Rust 1.75 Released"),
            syndication_title_key("Rust 175 Released")
        );
        // Identical up to case/whitespace still collapses (the target).
        assert_eq!(
            syndication_title_key("Breaking  News"),
            syndication_title_key("breaking news")
        );

        // End to end through merge: two different languages, different
        // domains, punctuation-only title difference, both must survive.
        let mut cpp = hit("https://cppreference.com/cpp", 0);
        cpp.title = "C++ Tutorial".into();
        let mut csharp = hit("https://learn.microsoft.com/csharp", 0);
        csharp.title = "C# Tutorial".into();
        let per = vec![
            ("bing".to_string(), vec![cpp]),
            ("mojeek".to_string(), vec![csharp]),
        ];
        let trust = std::collections::HashMap::new();
        let out = merge(
            &per,
            "tutorial",
            Intent::Code,
            &trust,
            &Families::default(),
            10,
        );
        let urls: Vec<&str> = out.iter().map(|m| m.url.as_str()).collect();
        assert!(
            urls.contains(&"https://cppreference.com/cpp")
                && urls.contains(&"https://learn.microsoft.com/csharp"),
            "both distinct-language results must survive: {urls:?}"
        );
    }

    #[test]
    fn general_engine_beats_vertical_only_same_consensus() {
        // The real-world bug: both results have consensus=1
        // (ddg only = 1 family, github only = 1 vertical),
        // so the consensus multiplier doesn't differentiate.
        // The vertical-only penalty must ensure the general
        // engine result wins despite the vertical having
        // higher BM25 (all query terms in its snippet).
        let mut gh_hit = hit("https://github.com/zigsafe", 0);
        gh_hit.title = "zigsafe: ownership checker for Zig, Rust-style borrow-check".into();
        gh_hit.snippet =
            "Optional static ownership checker for Zig with Rust-style borrow-check diagnostics"
                .into();
        let mut docs_hit = hit("https://doc.rust-lang.org/borrow", 3);
        docs_hit.title = "Borrowing - Rust By Example".into();
        docs_hit.snippet =
            "Rust uses a borrowing mechanism to access data without taking ownership".into();
        let per = vec![
            ("ddg".to_string(), vec![docs_hit]),
            ("github".to_string(), vec![gh_hit]),
        ];
        let trust = std::collections::HashMap::new();
        let out = merge(
            &per,
            "rust ownership borrow checker",
            Intent::Code,
            &trust,
            &Families::default(),
            10,
        );
        assert_eq!(
            out[0].url, "https://doc.rust-lang.org/borrow",
            "general engine result must beat vertical-only even with same consensus"
        );
    }

    #[test]
    fn diversity_caps_domains() {
        // other.com ranks higher than any same.com, so a
        // naive merge would still flood the top with
        // same.com #1..5 below it. Cap: max 2 per domain
        // before other domains get their slots; overflow
        // only backfills when the list runs short.
        let mut hits: Vec<Hit> = vec![hit("https://other.com/a", 0)];
        hits.extend((0..5).map(|i| hit(&format!("https://same.com/p{i}"), i + 1)));
        hits.push(hit("https://third.com/z", 9));
        let per = vec![("brave".to_string(), hits)];
        let trust = std::collections::HashMap::new();
        let out = merge(
            &per,
            "rust async runtime",
            Intent::Web,
            &trust,
            &Families::default(),
            10,
        );
        // third.com must appear before the 3rd same.com hit.
        let pos_third = out
            .iter()
            .position(|r| r.url.contains("third.com"))
            .unwrap();
        let third_same = out
            .iter()
            .enumerate()
            .filter(|(_, r)| r.url.contains("same.com"))
            .nth(2)
            .map(|(i, _)| i);
        if let Some(p3) = third_same {
            assert!(
                pos_third < p3,
                "diversity violated: third@{pos_third} same#3@{p3}"
            );
        }
    }

    #[test]
    fn norm_key_unifies_variants() {
        let a = norm_key("https://www.docs.rs/ratatui/index.html");
        let b = norm_key("http://docs.rs/ratatui/");
        assert_eq!(a, b);
    }

    #[test]
    fn norm_key_makes_one_arxiv_paper_of_its_abs_pdf_and_html_forms() {
        let abs = norm_key("https://arxiv.org/abs/1706.03762");
        assert_eq!(abs, norm_key("https://arxiv.org/pdf/1706.03762.pdf"));
        assert_eq!(abs, norm_key("http://www.arxiv.org/html/1706.03762"));
        assert_ne!(abs, norm_key("https://arxiv.org/abs/1706.03763"));
        assert_ne!(abs, norm_key("https://example.org/pdf/1706.03762.pdf"));
    }

    #[test]
    fn norm_key_drops_tracking_but_preserves_meaningful_query_parameters() {
        let tracked = norm_key(
            "https://example.com/search?topic=rust&utm_source=newsletter&page=2&gclid=abc",
        );
        let clean = norm_key("https://example.com/search?topic=rust&page=2");
        let different_page = norm_key("https://example.com/search?topic=rust&page=3");

        assert_eq!(tracked, clean);
        assert_ne!(clean, different_page);
    }

    #[test]
    fn norm_key_preserves_meaningful_query_order_and_encoding() {
        assert_ne!(
            norm_key("https://example.com/workflow?step=one&step=two"),
            norm_key("https://example.com/workflow?step=two&step=one")
        );
        assert_ne!(
            norm_key("https://example.com/search?q%3Da=b"),
            norm_key("https://example.com/search?q=a%3Db")
        );
    }

    #[test]
    fn norm_key_detects_encoded_and_case_insensitive_tracking_keys() {
        let clean = norm_key("https://example.com/page?id=7");
        assert_eq!(
            norm_key("https://example.com/page?%75tm_source=x&id=7&FBCLID=y"),
            clean
        );
        assert_ne!(norm_key("https://example.com/page?ref=docs&id=7"), clean);
    }

    #[test]
    fn empty_merge_is_safe() {
        let trust = std::collections::HashMap::new();
        let out = merge(
            &[],
            "anything",
            Intent::Web,
            &trust,
            &Families::default(),
            10,
        );
        assert!(out.is_empty());
    }

    #[test]
    fn a_snippet_is_bounded_whatever_the_source_sends() {
        // A 100 KB feed description used to reach the cross-encoder, which pads
        // every pair in its batch to the longest one: 40 CPU-seconds for one
        // search. The merge is the one place every source flows through.
        let huge = "x".repeat(100_000);
        let mut a = hit("https://example.com/a", 0);
        a.snippet = huge.clone();
        let per = vec![("bing".to_string(), vec![a])];
        let trust = std::collections::HashMap::new();
        let out = merge(
            &per,
            "example",
            Intent::Web,
            &trust,
            &Families::default(),
            10,
        );
        assert_eq!(out.len(), 1);
        assert!(
            out[0].snippet.chars().count() <= MAX_SNIPPET_CHARS,
            "snippet is {} chars",
            out[0].snippet.chars().count()
        );
    }

    #[test]
    fn whitespace_is_collapsed_in_title_and_snippet() {
        // JSON-sourced hits (MDN summaries, BYOK page text,
        // GitHub descriptions) arrive with raw newlines. The
        // markdown list indents snippets by three spaces, so an
        // embedded newline breaks the layout.
        let mut raw = hit("https://developer.mozilla.org/en-US/docs/Web/API/fetch", 0);
        raw.title = "fetch()\n  global function".into();
        raw.snippet = "Starts the process\nof fetching a resource,\treturning a promise.".into();
        let per = vec![("mdn".to_string(), vec![raw])];
        let trust = std::collections::HashMap::new();
        let out = merge(
            &per,
            "fetch api",
            Intent::Web,
            &trust,
            &Families::default(),
            10,
        );
        assert_eq!(out[0].title, "fetch() global function");
        assert_eq!(
            out[0].snippet,
            "Starts the process of fetching a resource, returning a promise."
        );
    }

    #[test]
    fn longest_snippet_wins_after_collapsing() {
        // Length comparison must run on collapsed strings: a
        // padded-with-newlines short snippet must not beat a
        // genuinely longer one.
        let mut padded = hit("https://a.com/x", 0);
        padded.snippet = "short\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n".into();
        let mut real = hit("https://a.com/x", 1);
        real.snippet = "a genuinely longer and more informative snippet".into();
        let per = vec![
            ("brave".to_string(), vec![padded]),
            ("ddg".to_string(), vec![real]),
        ];
        let trust = std::collections::HashMap::new();
        let out = merge(&per, "x", Intent::Web, &trust, &Families::default(), 10);
        assert_eq!(
            out[0].snippet,
            "a genuinely longer and more informative snippet"
        );
    }

    #[test]
    fn title_prefers_clean_over_breadcrumb() {
        let mut dirty = hit("https://en.wikipedia.org/wiki/Rust", 0);
        dirty.title = "en.wikipedia.org › wiki › Rust".into();
        let mut clean = hit("https://en.wikipedia.org/wiki/Rust", 3);
        clean.title = "Rust - Wikipedia".into();
        let per = vec![
            ("brave".to_string(), vec![dirty]),
            ("ddg".to_string(), vec![clean]),
        ];
        let trust = std::collections::HashMap::new();
        let out = merge(&per, "rust", Intent::Web, &trust, &Families::default(), 10);
        assert_eq!(out[0].title, "Rust - Wikipedia");
    }
}
