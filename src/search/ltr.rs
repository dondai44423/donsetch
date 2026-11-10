//! The learned ranker: eighteen features, one linear model, and the reason each
//! feature is there.
//!
//! There is no model runtime here. The weights are trained offline on the bench
//! (`scripts/train_rank.py`), compiled into [`super::rank_weights`] with the
//! corpus hash and the seed that produced them, and applied as a dot product —
//! microseconds, no dependency, and readable: `search --explain` prints every
//! feature and what its weight contributed.
//!
//! A model that does not beat the fusion it replaces on **held-out** rows is not
//! shipped; [`super::rank_weights::trained`] is false until the script writes
//! weights, and the ranker then stays on the measured fusion.

use super::query::{QueryFacts, Shape};
use super::rank::Merged;

/// How many features a result carries. Fixed by the trained model: changing it
/// invalidates every weight file, which the header of `rank_weights.rs` records.
pub const FEATURES: usize = 18;

/// What kind of page a URL is. The ranker learns how much each kind is worth for
/// each query shape instead of being told.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Documentation, an encyclopedia, a registry record: a page that *is* the
    /// answer to a name or a concept.
    Record,
    /// A publisher: a news site, a magazine, a vendor's own announcement.
    Publisher,
    /// A forum, a Q&A site, a discussion.
    Community,
    /// A page of links (a SERP, a directory, a tag page).
    Aggregator,
    /// Anything else.
    Other,
}

impl Kind {
    pub fn label(self) -> &'static str {
        match self {
            Kind::Record => "record",
            Kind::Publisher => "publisher",
            Kind::Community => "community",
            Kind::Aggregator => "aggregator",
            Kind::Other => "other",
        }
    }

    fn matches(self, expected: &[&str]) -> bool {
        expected.contains(&self.label())
    }
}

/// Hosts that publish a *record*: the page that is the thing, not a page about
/// it. Docs, registries, encyclopedias.
const RECORD_HOSTS: &[&str] = &[
    "docs.rs",
    "doc.rust-lang.org",
    "developer.mozilla.org",
    "docs.python.org",
    "nodejs.org",
    "react.dev",
    "angular.dev",
    "vuejs.org",
    "kubernetes.io",
    "postgresql.org",
    "sqlite.org",
    "git-scm.com",
    "man7.org",
    "crates.io",
    "npmjs.com",
    "pypi.org",
    "pkg.go.dev",
    "go.dev",
    "rust-lang.org",
    "wikipedia.org",
    "britannica.com",
    "ietf.org",
    "rfc-editor.org",
    "arxiv.org",
    "openalex.org",
    "nature.com",
    "acm.org",
    "ieee.org",
    "nih.gov",
    "who.int",
];

/// Hosts that host a conversation rather than a record.
const COMMUNITY_HOSTS: &[&str] = &[
    "stackoverflow.com",
    "stackexchange.com",
    "superuser.com",
    "serverfault.com",
    "askubuntu.com",
    "news.ycombinator.com",
    "reddit.com",
    "discourse.org",
    "github.com",
    "gitlab.com",
    "lobste.rs",
    "dev.to",
    "medium.com",
    "quora.com",
];

/// Hosts that publish their own material: news, magazines, vendors, blogs.
/// A page here is authoritative about what it says and about nothing else.
const PUBLISHER_HOSTS: &[&str] = &[
    "reuters.com",
    "apnews.com",
    "bbc.com",
    "bbc.co.uk",
    "theguardian.com",
    "nytimes.com",
    "washingtonpost.com",
    "wsj.com",
    "bloomberg.com",
    "ft.com",
    "arstechnica.com",
    "theverge.com",
    "techcrunch.com",
    "wired.com",
    "engadget.com",
    "cnet.com",
    "zdnet.com",
    "tomshardware.com",
    "anandtech.com",
    "phoronix.com",
    "theregister.com",
    "hackernews.com",
    "spacex.com",
    "nasa.gov",
    "tesla.com",
    "microsoft.com",
    "apple.com",
    "googleblog.com",
    "aws.amazon.com",
    "cloud.google.com",
    "openai.com",
    "anthropic.com",
    "tokio.rs",
    "serde.rs",
    "rust-lang.github.io",
];

/// Hosts whose page is a list of links.
const AGGREGATOR_HOSTS: &[&str] = &[
    "google.com",
    "bing.com",
    "yahoo.com",
    "duckduckgo.com",
    "pinterest.com",
    "youtube.com",
    "facebook.com",
    "twitter.com",
    "x.com",
    "linkedin.com",
    "tripadvisor.com",
    "amazon.com",
    "ebay.com",
    "etsy.com",
];

fn host_of(url: &str) -> String {
    url::Url::parse(url)
        .ok()
        .and_then(|u| {
            u.host_str()
                .map(|h| h.trim_start_matches("www.").to_ascii_lowercase())
        })
        .unwrap_or_default()
}

/// Any host matches itself or a subdomain of itself.
fn host_in(host: &str, list: &[&str]) -> bool {
    list.iter()
        .any(|h| host == *h || host.ends_with(&format!(".{h}")))
}

/// What kind of page this URL is.
pub fn kind(url: &str) -> Kind {
    let host = host_of(url);
    if host.is_empty() {
        return Kind::Other;
    }
    // Order matters: a GitHub *issue* is a conversation, a GitHub repo page is a
    // record; an amazon product page is not an aggregator.
    if host_in(&host, AGGREGATOR_HOSTS) && !host.contains("aws.amazon.com") {
        return Kind::Aggregator;
    }
    if host == "github.com" || host == "gitlab.com" {
        let path = url::Url::parse(url)
            .map(|u| u.path().to_string())
            .unwrap_or_default();
        let segments = path.split('/').filter(|s| !s.is_empty()).count();
        // `/owner/repo` is a record; `/owner/repo/issues/123` is a conversation.
        return if segments <= 2 {
            Kind::Record
        } else {
            Kind::Community
        };
    }
    if host_in(&host, COMMUNITY_HOSTS) {
        return Kind::Community;
    }
    if host_in(&host, RECORD_HOSTS) {
        return Kind::Record;
    }
    if host_in(&host, PUBLISHER_HOSTS) {
        return Kind::Publisher;
    }
    // A rustdoc or docs.* page anywhere is a record: the shape of the URL is
    // the site's own statement about what it published.
    if host.starts_with("docs.")
        || host.starts_with("doc.")
        || url::Url::parse(url)
            .map(|u| u.path().contains("/docs/") || u.path().contains("/reference/"))
            .unwrap_or(false)
    {
        return Kind::Record;
    }
    Kind::Other
}

/// Character 3–5-gram cosine between the query and a text: morphology and typos
/// without a model.
fn ngram_cosine(query: &str, text: &str) -> f64 {
    let grams = |s: &str| -> std::collections::HashMap<String, f64> {
        let lower = s.to_lowercase();
        let chars: Vec<char> = lower.chars().filter(|c| !c.is_whitespace()).collect();
        let mut out: std::collections::HashMap<String, f64> = std::collections::HashMap::new();
        for n in 3..=5 {
            if chars.len() < n {
                continue;
            }
            for window in chars.windows(n) {
                *out.entry(window.iter().collect()).or_insert(0.0) += 1.0;
            }
        }
        out
    };
    let a = grams(query);
    let b = grams(text);
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let dot: f64 = a
        .iter()
        .map(|(g, x)| b.get(g).map(|y| x * y).unwrap_or(0.0))
        .sum();
    let na: f64 = a.values().map(|v| v * v).sum::<f64>().sqrt();
    let nb: f64 = b.values().map(|v| v * v).sum::<f64>().sqrt();
    if na == 0.0 || nb == 0.0 {
        return 0.0;
    }
    (dot / (na * nb)).clamp(0.0, 1.0)
}

/// The age of a dated item, in days, `None` when the item carries no date.
fn age_days(published: &Option<String>) -> Option<f64> {
    let stamp = published.as_deref()?;
    let day = super::contract::day_of(stamp)?;
    let today = super::contract::today();
    Some((today - day).max(0) as f64)
}

/// One result's feature vector, in the order [`FEATURES`] documents.
///
/// Everything here is a fact about the candidate and the query — no thresholds,
/// no weights. What each is worth is the model's business.
pub fn features(
    facts: &QueryFacts,
    items: &[Merged],
    trust: &std::collections::HashMap<String, f64>,
) -> Vec<[f64; FEATURES]> {
    let bm25 = super::fuse::lexical(facts, items);
    let rm3 = super::fuse::reweight(facts, items);
    let rm3_top = rm3.iter().copied().fold(0.0, f64::max);
    let window = super::contract::window_for(facts.raw());
    let ident = facts.identifier_parts().map(|(root, rest)| {
        let mut tokens = vec![root];
        tokens.extend(rest);
        tokens
    });
    let expected = facts.shape().expected_kinds();
    let mut out = Vec::with_capacity(items.len());
    for (i, item) in items.iter().enumerate() {
        let title_l = item.title.to_lowercase();
        let url_l = item.url.to_lowercase();
        let path = url::Url::parse(&item.url)
            .map(|u| u.path().to_string())
            .unwrap_or_default();
        let segments = path.split('/').filter(|s| !s.is_empty()).count() as f64;
        let kind = kind(&item.url);
        let (any_anchor, all_anchors) =
            facts.anchor_state(&format!("{} {} {}", item.title, item.snippet, item.url));
        let anchor = if facts.anchors().is_empty() || all_anchors {
            1.0
        } else if any_anchor {
            0.5
        } else {
            0.0
        };
        // The best engine that returned this, by learned trust for this intent.
        let shape_trust = item
            .sources
            .iter()
            .map(|(engine, _)| {
                trust
                    .get(super::egress::health_key(engine))
                    .copied()
                    .unwrap_or(1.0)
            })
            .fold(0.0_f64, f64::max);
        let best_rank = item
            .sources
            .iter()
            .map(|(_, rank)| *rank)
            .min()
            .map(|r| 1.0 / (1.0 + r as f64))
            .unwrap_or(0.0);
        let snippet_q = if item.snippet.trim().is_empty() {
            0.0
        } else if item.snippet.chars().count() >= 80 {
            1.0
        } else {
            0.5
        };
        let fresh = match (window.is_some(), age_days(&item.published)) {
            (true, Some(days)) => 1.0 / (1.0 + days / 30.0),
            _ => 0.0,
        };
        // The identifier is a *path*: each segment that appears in the title or
        // the URL is one share of the match, so `itertools::izip` scores 1.0 on
        // docs.rs/itertools/…/macro.izip.html without needing the joined string.
        let ident_hit = ident
            .as_ref()
            .map(|tokens| {
                let found = tokens
                    .iter()
                    .filter(|t| {
                        let t = t.to_lowercase();
                        title_l.contains(&t) || url_l.contains(&t)
                    })
                    .count();
                found as f64 / tokens.len() as f64
            })
            .unwrap_or(0.0);
        let mut x = [0.0; FEATURES];
        // 1–3: lexical evidence per field, from the same BM25+ the fusion uses.
        // The split is by field weight, so the model can learn that a title match
        // is worth more than a URL match for one shape and less for another.
        x[0] = bm25[i] * 3.0;
        x[1] = bm25[i];
        x[2] = bm25[i] * 1.5;
        // 4–6: agreement and rank.
        x[3] = item.families as f64;
        x[4] = best_rank;
        x[5] = shape_trust;
        // 7–9: result kind.
        x[6] = f64::from(kind == Kind::Record);
        x[7] = f64::from(kind == Kind::Publisher);
        x[8] = f64::from(kind == Kind::Aggregator);
        // 10–14: freshness, anchors, the identifier itself, expansion, morphology.
        x[9] = fresh;
        x[10] = anchor;
        x[11] = ident_hit;
        x[12] = if rm3_top > 0.0 { rm3[i] / rm3_top } else { 0.0 };
        x[13] = ngram_cosine(facts.raw(), &format!("{} {}", item.title, item.snippet));
        // 15–18: snippet quality, URL depth, shape match, bias.
        x[14] = snippet_q;
        x[15] = 1.0 / (1.0 + segments);
        x[16] = f64::from(kind.matches(expected));
        x[17] = 1.0;
        out.push(x);
    }
    out
}

/// The linear model: standardise each feature by the trained mean and spread,
/// then take the dot product.
pub fn score(weights: &super::rank_weights::Weights, x: &[f64; FEATURES]) -> f64 {
    let mut total = 0.0;
    for j in 0..FEATURES {
        let std = if weights.std[j].abs() < 1e-9 {
            1.0
        } else {
            weights.std[j]
        };
        total += weights.w[j] * ((x[j] - weights.mean[j]) / std);
    }
    total
}

/// One feature's contribution to a score: the same arithmetic, shown.
pub fn contribution(
    weights: &super::rank_weights::Weights,
    x: &[f64; FEATURES],
) -> [f64; FEATURES] {
    let mut out = [0.0; FEATURES];
    for j in 0..FEATURES {
        let std = if weights.std[j].abs() < 1e-9 {
            1.0
        } else {
            weights.std[j]
        };
        out[j] = weights.w[j] * ((x[j] - weights.mean[j]) / std);
    }
    out
}

/// The name of each feature, in order: what `--explain` prints.
pub const NAMES: [&str; FEATURES] = [
    "bm25_title",
    "bm25_snippet",
    "bm25_url",
    "families",
    "best_rank",
    "shape_trust",
    "kind_record",
    "kind_publisher",
    "kind_aggregator",
    "fresh",
    "anchor",
    "ident",
    "rm3",
    "ngram",
    "snippet_q",
    "depth",
    "shape_match",
    "bias",
];

/// The shape's expected kinds, as the model sees them.
pub fn expected(shape: Shape) -> &'static [&'static str] {
    shape.expected_kinds()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(url: &str, title: &str, snippet: &str) -> Merged {
        Merged {
            title: title.into(),
            url: url.into(),
            snippet: snippet.into(),
            sources: vec![("bing".into(), 0)],
            score: 0.0,
            published: None,
            families: 1,
            from_page: false,
            tier: 1,
        }
    }

    #[test]
    fn a_record_is_recognised_and_an_issue_is_not() {
        assert_eq!(kind("https://docs.rs/tokio/latest/tokio/"), Kind::Record);
        assert_eq!(
            kind("https://en.wikipedia.org/wiki/Quantum_entanglement"),
            Kind::Record
        );
        assert_eq!(kind("https://github.com/tokio-rs/tokio"), Kind::Record);
        assert_eq!(
            kind("https://github.com/tokio-rs/tokio/issues/1234"),
            Kind::Community
        );
        assert_eq!(
            kind("https://stackoverflow.com/questions/123/what"),
            Kind::Community
        );
        assert_eq!(
            kind("https://www.reuters.com/technology/x"),
            Kind::Publisher
        );
        assert_eq!(kind("https://www.google.com/search?q=x"), Kind::Aggregator);
        assert_eq!(kind("https://example.org/a"), Kind::Other);
    }

    #[test]
    fn the_feature_vector_is_the_model_shape() {
        let facts = QueryFacts::new("tokio spawn");
        let items = vec![
            m(
                "https://docs.rs/tokio/latest/tokio/task/fn.spawn.html",
                "spawn",
                "Spawns a task.",
            ),
            m("https://example.org/x", "Something else", ""),
        ];
        let trust = std::collections::HashMap::new();
        let f = features(&facts, &items, &trust);
        assert_eq!(f.len(), 2);
        assert_eq!(f[0].len(), FEATURES);
        // The docs page matches the query and is a record: both must show.
        assert!(f[0][0] > f[1][0], "bm25 title: {:?}", (f[0][0], f[1][0]));
        assert_eq!(f[0][6], 1.0, "kind_record");
        assert_eq!(f[0][17], 1.0, "bias is always 1");
        assert!(f[0][15] < 1.0, "depth is a fraction");
    }

    #[test]
    fn shape_match_follows_the_query_shape() {
        let facts = QueryFacts::new("itertools::izip");
        assert_eq!(facts.shape(), Shape::Identifier);
        let items = vec![m(
            "https://docs.rs/itertools/latest/itertools/macro.izip.html",
            "izip in itertools - Rust",
            "Create an iterator.",
        )];
        let f = features(&facts, &items, &std::collections::HashMap::new());
        assert_eq!(f[0][16], 1.0, "a record answers a name");
        assert_eq!(f[0][11], 1.0, "the identifier appears in the URL");
    }

    #[test]
    fn ngram_cosine_is_near_one_for_the_same_words() {
        let a = ngram_cosine("tokio spawn blocking", "spawn_blocking in tokio");
        let b = ngram_cosine("tokio spawn blocking", "unrelated weather report");
        assert!(a > b, "{a} vs {b}");
        assert!((0.0..=1.0).contains(&a));
    }

    #[test]
    fn contribution_sums_to_the_score() {
        let w = super::super::rank_weights::Weights::PLACEHOLDER;
        let facts = QueryFacts::new("x");
        let items = vec![m("https://docs.rs/x/latest/x/", "x", "x")];
        let x = features(&facts, &items, &std::collections::HashMap::new())[0];
        let total: f64 = contribution(&w, &x).iter().sum();
        assert!((total - score(&w, &x)).abs() < 1e-9);
    }
}
