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
            | "docs"
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
        return raw.to_string();
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
        // `2005.11401v4` is a version of `2005.11401`: one paper, one row.
        .map(|id| {
            let unversioned = id
                .rsplit_once('v')
                .filter(|(stem, n)| {
                    !stem.is_empty() && !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit())
                })
                .map_or(id, |(stem, _)| stem);
            format!("/abs/{unversioned}")
        });
    let path = arxiv_id.unwrap_or_else(|| u.path().to_string());
    let path = path
        .trim_end_matches('/')
        .trim_end_matches("/index.html")
        .trim_end_matches("/index.htm")
        .to_string();
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
    let port = u.port().map(|p| format!(":{p}")).unwrap_or_default();
    format!("{host}{port}{path}{query}")
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

/// No snippet may exceed this, whatever the source. The render cuts to 180 and
/// every candidate is batched to the longest snippet it is given, so one
/// unbounded description costs work on every search.
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
    let mut candidates = pool(per_engine, query, intent, trust, families);
    order(&mut candidates, model());
    finish(candidates, max_results)
}

/// The exact inputs of one merge. The live search keeps the last one so
/// `search-bench` can tell a retrieval miss from an ordering miss and
/// `search --explain` can re-rank the very same candidates.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct MergeInput {
    pub per_engine: Vec<(String, Vec<Hit>)>,
    pub query: String,
    pub intent: Intent,
    pub trust: HashMap<String, f64>,
    pub families: Families,
    pub depth: usize,
    /// Observed deltas from explicit feedback/page-verification steps. Empty
    /// for the default pipeline; older captures replay without these steps.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub post_adjustments: HashMap<String, f64>,
}

impl MergeInput {
    pub fn merge(&self) -> Vec<Merged> {
        let mut results = merge(
            &self.per_engine,
            &self.query,
            self.intent,
            &self.trust,
            &self.families,
            self.depth,
        );
        self.apply_post_adjustments(&mut results);
        results
    }

    pub(crate) fn apply_post_adjustments(&self, results: &mut [Merged]) {
        for result in results.iter_mut() {
            result.score += self
                .post_adjustments
                .get(&norm_key(&result.url))
                .copied()
                .unwrap_or(0.0);
        }
        results.sort_by(|a, b| {
            a.tier
                .cmp(&b.tier)
                .then_with(|| b.score.total_cmp(&a.score))
        });
    }
}

/// One merged result with the evidence that placed it: its tier, the score it
/// was sorted by, its feature vector and — when the learned ranker is installed
/// — each feature's share of that score.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Explained {
    pub url: String,
    pub tier: u8,
    pub score: f64,
    pub x: [f64; super::ltr::FEATURES],
    pub parts: Option<[f64; super::ltr::FEATURES]>,
    pub post_adjustment: f64,
}

/// The merge, explained: the same pool, order and cut as [`merge`], with each
/// result's features. Features are computed over the whole pool before it is
/// ordered, exactly as the ranker computes them.
pub fn explain(input: &MergeInput) -> Vec<Explained> {
    let mut candidates = pool(
        &input.per_engine,
        &input.query,
        input.intent,
        &input.trust,
        &input.families,
    );
    let vectors: HashMap<String, [f64; super::ltr::FEATURES]> = candidates
        .items
        .iter()
        .zip(super::ltr::features(&candidates.facts, &candidates.items))
        .map(|(m, x)| (norm_key(&m.url), x))
        .collect();
    let weights = model().filter(|w| w.ranks(candidates.facts.shape()));
    order(&mut candidates, weights);
    let placed: HashMap<String, (u8, f64)> = candidates
        .items
        .iter()
        .map(|m| (norm_key(&m.url), (m.tier, m.score)))
        .collect();
    let mut results = finish(candidates, input.depth);
    input.apply_post_adjustments(&mut results);
    results
        .into_iter()
        .map(|m| {
            let key = norm_key(&m.url);
            let x = vectors
                .get(&key)
                .copied()
                .unwrap_or([0.0; super::ltr::FEATURES]);
            let (tier, _) = placed.get(&key).copied().unwrap_or((m.tier, m.score));
            Explained {
                url: m.url,
                tier,
                score: m.score,
                x,
                parts: weights.map(|w| super::ltr::contribution(w, &x)),
                post_adjustment: input.post_adjustments.get(&key).copied().unwrap_or(0.0),
            }
        })
        .collect()
}

/// The installed ranker, when one is trained and not switched off
/// (`search.learned_rank`). `None` ranks by the fusion alone.
pub(crate) fn model() -> Option<&'static super::rank_weights::Weights> {
    (super::rank_weights::trained() && crate::config::cfg().search.learned_rank)
        .then_some(&super::rank_weights::WEIGHTS)
}

/// Every merged candidate for one query, scored by the fusion and classed by
/// tier, before any is cut. The live merge, the training dump and `--explain`
/// all read this one pool, so a feature vector is always computed over the
/// same set the ranker saw.
pub(crate) struct Pool {
    pub items: Vec<Merged>,
    pub facts: super::query::QueryFacts,
    query: String,
    /// Every source's snippet per URL key: the shown one is picked once the
    /// order is settled (`evidence::pick`), so ranking never reads a cut text.
    snippets: HashMap<String, Vec<String>>,
    /// Answer excerpts already selected from a first-party response. Keep
    /// their source line breaks instead of treating code as a SERP caption.
    quotes: HashMap<String, String>,
}

/// Fusion over measured independence, and nothing else *in this layer*.
///
/// A URL's score here is the best rank-decayed vote of each independent family
/// that returned it, lifted a little for every extra family. Text
/// arrives one layer up, in [`super::fuse`], which blends BM25+ over the merged
/// candidates with this vote — the vote knows nothing about text on purpose.
///
/// Measured on a 40-query gold set with the sources' own results held fixed: a
/// domain prior and an authority multiplier took hit@1 from 0.62 down to 0.17,
/// because the additive terms were ten to twenty times the size of the rank
/// signal and promoted exactly the pages a search engine had already learned to
/// bury (SEO restatements of the query). What survives here is a bounded
/// coverage penalty for wrong versions and mangled compound terms, and a
/// freshness lift for dated news items.
pub(crate) fn pool(
    per_engine: &[(String, Vec<Hit>)],
    query: &str,
    intent: Intent,
    trust: &HashMap<String, f64>,
    families: &Families,
) -> Pool {
    let facts = super::query::QueryFacts::new(query);
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
    let mut quotes = HashMap::new();
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
            if super::contract::veto(&facts, hit).is_some() {
                continue;
            }
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
            if engine == "stackexchange"
                && (hit.snippet.starts_with("Accepted answer: ")
                    || hit.snippet.starts_with("Answer: "))
            {
                quotes
                    .entry(key.clone())
                    .or_insert_with(|| hit.snippet.chars().take(MAX_SNIPPET_CHARS).collect());
            }
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
            // Canonical views share votes, but an explicit format must keep
            // the matching URL when that view actually appeared in a source.
            // Never synthesize a PDF address from an abstract-page address.
            if let Some(format) = &facts.compiled().filetype
                && super::query::url_filetype(&hit.url).as_ref() == Some(format)
                && super::query::url_filetype(&entry.url).as_ref() != Some(format)
            {
                entry.url = hit.url.clone();
            }
            // Keep the longest snippet (most informative), skipping redirect
            // stubs. Both sides are already bounded (MAX_SNIPPET_CHARS).
            if hit_snippet.len() > entry.snippet.len() && !hit_snippet.starts_with("Redirecting") {
                entry.snippet = hit_snippet.clone();
            }
            // Best title: the CLEAN candidate that says most of what the query
            // asked, then the shortest (breadcrumbs "a › b › c" and URL echoes
            // are longer than real titles). A PDF's header line is not a title:
            // "arXiv:2005.11401v4 [cs.CL] 12 Apr 2021" was the shortest
            // candidate for the RAG paper and won over its real name.
            let bad = |t: &str| {
                t.contains(" › ")
                    || t.starts_with("http")
                    || t.len() < 3
                    || t.to_ascii_lowercase().starts_with("arxiv:")
            };
            let better = || {
                let (new, old) = (
                    super::evidence::coverage(query, &hit_title),
                    super::evidence::coverage(query, &entry.title),
                );
                new > old || (new == old && hit_title.len() < entry.title.len())
            };
            if !bad(&hit_title) && (bad(&entry.title) || better()) {
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
    // it's probably tangential. The `authoritative` flag already gives full RRF
    // weight (no VERTICAL_WEIGHT reduction), and the penalty for zero
    // general-engine corroboration applies equally: if no SERP found the
    // Wikipedia article, it's probably the wrong one (e.g. Linearizability for
    // a CRDT query).
    for r in &mut results {
        let has_general = r.sources.iter().any(|(e, _)| !is_vertical(e));
        if !has_general {
            r.score *= 0.4;
        }
    }

    // Entity coverage penalty: penalize results that miss
    // key query entities (compound terms like "B-tree", wrong
    // version numbers like "5.5" when the query says "5.2").
    // Coverage gaps cannot be overridden by lexical evidence:
    // "binary tree" ≠ "B-tree" no matter how well the words match.
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
    for r in &mut results {
        let resolved = r
            .sources
            .iter()
            .any(|(engine, _)| engine == "registry" || engine == "docs");
        let record = intent == Intent::Code
            && super::contract::package_scope_named(&facts, &r.url)
            && facts.is_exact(&r.title, &r.snippet, &r.url);
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

    // The evidence, then the tie break. The vote is what independent indexes
    // agree on; the lexical score is what the answers themselves say about the
    // query. The vote leads and the evidence may move a candidate by at most the
    // tie window — an even blend re-promoted SEO restatements of the query over
    // the pages three indexes already rank well. Tiers still sort first: a
    // resolver's record is proof the query named the page.
    {
        let vote: Vec<f64> = results.iter().map(|r| r.score).collect();
        let evidence = super::fuse::reweight(&facts, &results);
        for (r, fused) in results.iter_mut().zip(super::fuse::fuse(
            &vote,
            &evidence,
            crate::config::cfg().search.tie_window,
        )) {
            r.score = fused;
        }
    }
    Pool {
        items: results,
        facts,
        query: query.to_string(),
        snippets,
        quotes,
    }
}

/// Order the pool: tiers first, then the learned ranker's score when one is
/// installed and ranks this query's shape, else the fusion's. The model replaces the score, never the tier —
/// a resolver's record is proof the query named the page.
pub(crate) fn order(pool: &mut Pool, model: Option<&super::rank_weights::Weights>) {
    if let Some(weights) = model.filter(|w| w.ranks(pool.facts.shape())) {
        let x = super::ltr::features(&pool.facts, &pool.items);
        for (r, x) in pool.items.iter_mut().zip(&x) {
            r.score = super::ltr::score(weights, x);
        }
    }
    // Stable: candidates the score cannot separate keep first-seen order, so a
    // tie resolves the same way on every run.
    pool.items.sort_by(|a, b| {
        a.tier
            .cmp(&b.tier)
            .then_with(|| b.score.total_cmp(&a.score))
    });
}

/// The answer from an ordered pool: syndication dedup, diversity, the result
/// cap, and the snippet each result shows.
pub(crate) fn finish(pool: Pool, max_results: usize) -> Vec<Merged> {
    let Pool {
        items: mut results,
        facts,
        query,
        snippets,
        quotes,
    } = pool;
    let query = query.as_str();
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

    // Diversity: two per host, one per canonical page, in fused order. A host
    // past its cap fills an empty slot only with a page that is itself about
    // the query: its own title and URL cover it, not a snippet that mentions
    // it. Filling with whatever was left put the subject's father and a mascot
    // page into "who is Linus Torvalds" as the third and fourth Wikipedia rows;
    // a shorter list costs the reader less.
    let (mut diverse, overflow) = super::fuse::diversify(results, max_results);
    if diverse.len() < max_results {
        let room = max_results - diverse.len();
        diverse.extend(
            overflow
                .into_iter()
                .filter(|r| facts.is_exact(&r.title, "", &r.url))
                .take(room),
        );
    }
    diverse.truncate(max_results);
    for (position, r) in diverse.iter_mut().enumerate() {
        let key = norm_key(&r.url);
        if let Some(quote) = quotes.get(&key) {
            r.snippet = super::evidence::quote(quote, super::evidence::budget(position));
        } else if let Some(candidates) = snippets.get(&key) {
            r.snippet = super::evidence::pick(query, candidates, super::evidence::budget(position));
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

    #[test]
    fn explain_shows_the_merge_it_explains() {
        let per = vec![
            (
                "bing".to_string(),
                (0..6)
                    .map(|i| hit(&format!("https://h{i}.example/rust-async"), i))
                    .collect(),
            ),
            (
                "ddg".to_string(),
                vec![hit("https://h3.example/rust-async", 0)],
            ),
            ("github".to_string(), vec![hit("https://gh.example/r", 0)]),
        ];
        let mut input = MergeInput {
            per_engine: per,
            query: "rust async runtime".into(),
            intent: Intent::Code,
            trust: HashMap::new(),
            families: Families::default(),
            depth: 5,
            post_adjustments: HashMap::new(),
        };
        let first = input.merge()[0].url.clone();
        for adjusted in [false, true] {
            if adjusted {
                input.post_adjustments.insert(norm_key(&first), -0.25);
            }
            let merged: Vec<String> = input.merge().into_iter().map(|m| m.url).collect();
            let explained = explain(&input);
            let urls: Vec<String> = explained.iter().map(|e| e.url.clone()).collect();
            assert_eq!(urls, merged, "the same results, in the same order");
            assert!(
                explained
                    .windows(2)
                    .all(|w| (w[0].tier, -w[0].score) <= (w[1].tier, -w[1].score)),
                "the scores give the order shown"
            );
            if let Some(parts) = &explained[0].parts {
                let total: f64 = parts.iter().sum();
                assert!(
                    (total + explained[0].post_adjustment - explained[0].score).abs() < 1e-9,
                    "shares sum to the score"
                );
            }
            assert_eq!(
                explained
                    .iter()
                    .find(|e| e.url == first)
                    .unwrap()
                    .post_adjustment,
                if adjusted { -0.25 } else { 0.0 }
            );
        }
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

    /// Past its cap a host refills an empty slot only with a page whose own
    /// title is about the query; a page that merely mentions it in a snippet
    /// stays out, and the list is shorter instead.
    #[test]
    fn an_over_cap_host_refills_only_with_pages_about_the_query() {
        let page = |title: &str, slug: &str, rank: usize| Hit {
            title: title.into(),
            url: format!("https://en.wikipedia.org/wiki/{slug}"),
            snippet: "the son of Ole Torvalds and the father of Linus Torvalds".into(),
            rank,
            published: None,
        };
        let per = vec![(
            "wikipedia".to_string(),
            vec![
                page("Linus Torvalds", "Linus_Torvalds", 0),
                page("History of Linux", "History_of_Linux", 1),
                page("Nils Torvalds", "Nils_Torvalds", 2),
                page("Tux (mascot)", "Tux_(mascot)", 3),
                page(
                    "Linus Torvalds bibliography",
                    "Linus_Torvalds_bibliography",
                    4,
                ),
            ],
        )];
        let out = merge(
            &per,
            "who is Linus Torvalds",
            Intent::Entity,
            &std::collections::HashMap::new(),
            &Families::default(),
            7,
        );
        let titles: Vec<&str> = out.iter().map(|r| r.title.as_str()).collect();
        assert!(
            titles.contains(&"Linus Torvalds bibliography"),
            "{titles:?}"
        );
        assert!(!titles.contains(&"Nils Torvalds"), "{titles:?}");
        assert!(!titles.contains(&"Tux (mascot)"), "{titles:?}");
    }

    #[test]
    fn norm_key_unifies_variants() {
        let a = norm_key("https://www.docs.rs/ratatui/index.html");
        let b = norm_key("http://docs.rs/ratatui/");
        assert_eq!(a, b);
        assert_ne!(
            norm_key("https://example.com:8443/Type"),
            norm_key("https://example.com/Type")
        );
        assert_eq!(
            norm_key("https://example.com:443/Type"),
            norm_key("https://example.com/Type")
        );
        // HTTP paths and identifiers are case-sensitive; only hosts fold.
        assert_eq!(norm_key("https://EXAMPLE.com/Type"), "example.com/Type");
        assert_ne!(
            norm_key("https://example.com/Type"),
            norm_key("https://example.com/type")
        );
    }

    #[test]
    fn norm_key_makes_one_arxiv_paper_of_its_abs_pdf_and_html_forms() {
        let abs = norm_key("https://arxiv.org/abs/1706.03762");
        assert_eq!(abs, norm_key("https://arxiv.org/pdf/1706.03762.pdf"));
        assert_eq!(abs, norm_key("http://www.arxiv.org/html/1706.03762"));
        assert_ne!(abs, norm_key("https://arxiv.org/abs/1706.03763"));
        // A version is the same paper; an old-style id with a `v` in its
        // archive name is not a version.
        assert_eq!(abs, norm_key("https://arxiv.org/abs/1706.03762v7"));
        assert_eq!(abs, norm_key("https://arxiv.org/pdf/1706.03762v2.pdf"));
        assert_eq!(
            norm_key("https://arxiv.org/abs/solv-int/9901001"),
            "arxiv.org/abs/solv-int/9901001"
        );
        assert_eq!(
            norm_key("https://arxiv.org/abs/hep-th/9901001v3"),
            "arxiv.org/abs/hep-th/9901001"
        );
        assert_ne!(abs, norm_key("https://example.org/pdf/1706.03762.pdf"));
        let per = vec![(
            "bing".into(),
            vec![
                hit("https://arxiv.org/abs/1706.03762", 0),
                hit("https://arxiv.org/pdf/1706.03762.pdf", 1),
            ],
        )];
        let merged = merge(
            &per,
            "paper filetype:pdf",
            Intent::Paper,
            &HashMap::new(),
            &Families::default(),
            12,
        );
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].url, "https://arxiv.org/pdf/1706.03762.pdf");
        let mut extensionless = per.clone();
        extensionless[0].1[1].url = "https://arxiv.org/pdf/1706.03762".into();
        let merged = merge(
            &extensionless,
            "paper filetype:pdf",
            Intent::Paper,
            &HashMap::new(),
            &Families::default(),
            12,
        );
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].url, "https://arxiv.org/pdf/1706.03762");
        let ordinary = merge(
            &per,
            "paper",
            Intent::Paper,
            &HashMap::new(),
            &Families::default(),
            12,
        );
        assert_eq!(ordinary[0].url, "https://arxiv.org/abs/1706.03762");
        let abstract_only = vec![(
            "bing".into(),
            vec![hit("https://arxiv.org/abs/1706.03762", 0)],
        )];
        let unverified = merge(
            &abstract_only,
            "paper filetype:pdf",
            Intent::Paper,
            &HashMap::new(),
            &Families::default(),
            12,
        );
        assert_eq!(unverified[0].url, "https://arxiv.org/abs/1706.03762");
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
        let mut answer = hit("https://stackoverflow.com/questions/42/example", 0);
        answer.title = "Orion configuration".into();
        answer.snippet =
            "Accepted answer: Orion configuration:\nserver:\n    timeout: 42\n    retries: 3"
                .into();
        let quoted = merge(
            &[("stackexchange".into(), vec![answer.clone()])],
            "Orion configuration",
            Intent::Code,
            &trust,
            &Families::default(),
            12,
        );
        assert_eq!(quoted[0].snippet, answer.snippet);
        assert!(super::super::evidence::quote(&answer.snippet, 55).contains("\nserver:"));
        assert!(!super::super::evidence::quote(&answer.snippet, 55).contains("timeout: 4"));
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
