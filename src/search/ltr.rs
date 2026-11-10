//! The learned ranker: twenty-two features, one linear model, and the reason each
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

use super::query::QueryFacts;
use super::rank::Merged;

/// How many features a result carries. Fixed by the trained model: changing it
/// invalidates every weight file, which the header of `rank_weights.rs` records.
pub const FEATURES: usize = 22;

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

/// Does the site's own name say what the query asks about? `wireguard.com` for
/// "set up a wireguard server", `postgresql.org` for "postgres index only scan",
/// `docs.python.org` for "python asyncio gather". The host's labels (and their
/// `-` parts) are compared with the query's Latin words, the top-level domain
/// excluded; a label that starts with a word of four or more letters counts, so
/// "postgres" names `postgresql`. A fact about the URL, not a host list: the
/// model learns what it is worth, and an SEO domain that wears the word pays the
/// same feature with none of the agreement a first-party page earns.
fn host_named(facts: &QueryFacts, url: &str) -> f64 {
    let host = host_of(url);
    let mut labels: Vec<&str> = host.split('.').collect();
    labels.pop();
    let parts: Vec<&str> = labels.iter().flat_map(|l| l.split('-')).collect();
    let named = facts.terms().any(|term| {
        let n = term.chars().count();
        n >= 3
            && term.chars().all(|c| c.is_ascii_alphanumeric())
            && !super::query::is_stop_word(term)
            && parts
                .iter()
                .any(|p| *p == term || (n >= 4 && p.starts_with(term)))
    });
    f64::from(named)
}

/// One result's feature vector, in the order [`NAMES`] lists.
///
/// Everything here is a fact about the candidate, the query and the pool it was
/// merged into — no thresholds, no weights. What each is worth is the model's
/// business. Scores that depend on the query's length (the fusion's vote, BM25+
/// per field, RM3) are scaled against the pool's strongest candidate, so one
/// weight means the same thing on a two-word query and a twelve-word one.
pub fn features(facts: &QueryFacts, items: &[Merged]) -> Vec<[f64; FEATURES]> {
    use super::fuse::normalize;
    let vote = normalize(&items.iter().map(|m| m.score).collect::<Vec<_>>());
    let [title, snippet, url, host] = super::fuse::fields(facts, items).map(|f| normalize(&f));
    let rm3 = normalize(&super::fuse::reweight(facts, items));
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
        // A kind is evidence only where the query's shape asks for it: an
        // encyclopedia article is the answer to a name and a tangent to
        // `東京の天気予報`. Read as one global bonus, `record` lifted the
        // Wikipedia article over the weather service on every web query.
        let fits = kind.matches(expected);
        let (any_anchor, all_anchors) =
            facts.anchor_state(&format!("{} {} {}", item.title, item.snippet, item.url));
        let anchor = if facts.anchors().is_empty() || all_anchors {
            1.0
        } else if any_anchor {
            0.5
        } else {
            0.0
        };
        // Rank in a general index only. A vertical searches its own small
        // corpus and puts its own page first almost always, so its rank 1 is
        // not the evidence Bing's rank 1 is; `vertical_only` carries those.
        let best_rank = item
            .sources
            .iter()
            .filter(|(engine, _)| !super::rank::is_vertical(engine))
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
        out.push([
            vote[i],
            title[i],
            snippet[i],
            url[i],
            host[i],
            host_named(facts, &item.url),
            item.families.min(4) as f64,
            best_rank,
            f64::from(fits && kind == Kind::Record),
            f64::from(fits && kind == Kind::Publisher),
            f64::from(fits && kind == Kind::Community),
            f64::from(kind == Kind::Aggregator),
            fresh,
            anchor,
            ident_hit,
            rm3[i],
            ngram_cosine(facts.raw(), &format!("{} {}", item.title, item.snippet)),
            snippet_q,
            1.0 / (1.0 + segments),
            // A record, publisher or community page the shape does not ask for.
            f64::from(!fits && matches!(kind, Kind::Record | Kind::Publisher | Kind::Community)),
            // How much of the query this candidate says, in absolute terms: the
            // pool-relative BM25 above cannot tell a page that matches one of
            // four parts of the query from one that matches all four.
            facts.coverage(&format!("{} {} {}", item.title, item.snippet, item.url)),
            // Only a first-party vertical returned it: no general index agreed.
            f64::from(
                !item.sources.is_empty()
                    && item
                        .sources
                        .iter()
                        .all(|(engine, _)| super::rank::is_vertical(engine)),
            ),
        ]);
    }
    out
}

/// The standardised value of feature `j`: what the weight multiplies.
fn standard(weights: &super::rank_weights::Weights, x: &[f64; FEATURES], j: usize) -> f64 {
    let std = if weights.std[j].abs() < 1e-9 {
        1.0
    } else {
        weights.std[j]
    };
    (x[j] - weights.mean[j]) / std
}

/// The linear model: standardise each feature by the trained mean and spread,
/// then take the dot product.
pub fn score(weights: &super::rank_weights::Weights, x: &[f64; FEATURES]) -> f64 {
    contribution(weights, x).iter().sum()
}

/// Each feature's share of a score: the same arithmetic, shown. The shares sum
/// to [`score`] exactly.
pub fn contribution(
    weights: &super::rank_weights::Weights,
    x: &[f64; FEATURES],
) -> [f64; FEATURES] {
    std::array::from_fn(|j| weights.w[j] * standard(weights, x, j))
}

/// The name of each feature, in order: what `--explain` prints and what
/// `scripts/train_rank.py` writes beside each weight.
pub const NAMES: [&str; FEATURES] = [
    "vote",
    "bm25_title",
    "bm25_snippet",
    "bm25_url",
    "bm25_host",
    "host_named",
    "families",
    "best_rank",
    "kind_record",
    "kind_publisher",
    "kind_community",
    "kind_aggregator",
    "fresh",
    "anchor",
    "ident",
    "rm3",
    "ngram",
    "snippet_q",
    "depth",
    "off_shape",
    "coverage",
    "vertical_only",
];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search::query::Shape;

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
        let f = features(&facts, &items);
        assert_eq!(f.len(), 2);
        assert_eq!(f[0].len(), FEATURES);
        // The docs page matches the query and is a record: both must show.
        assert!(f[0][1] > f[1][1], "bm25 title: {:?}", (f[0][1], f[1][1]));
        assert_eq!(f[0][8] + f[0][19], 1.0, "a record, on the shape or off it");
        assert!(f[0][18] < 1.0, "depth is a fraction");
        // Pool-relative scores top out at 1 on the strongest candidate.
        assert!(f.iter().all(|x| (0.0..=1.0).contains(&x[1])));
    }

    #[test]
    fn a_kind_counts_only_where_the_shape_asks_for_it() {
        let facts = QueryFacts::new("itertools::izip");
        assert_eq!(facts.shape(), Shape::Identifier);
        let items = vec![m(
            "https://docs.rs/itertools/latest/itertools/macro.izip.html",
            "izip in itertools - Rust",
            "Create an iterator.",
        )];
        let f = features(&facts, &items);
        assert_eq!(f[0][8], 1.0, "a record answers a name");
        assert_eq!(f[0][19], 0.0, "and is on shape");
        assert_eq!(f[0][14], 1.0, "the identifier appears in the URL");

        let facts = QueryFacts::new("東京の天気予報");
        assert_eq!(facts.shape(), Shape::Web);
        let wiki = m(
            "https://ja.wikipedia.org/wiki/天気予報",
            "天気予報 - Wikipedia",
            "天気予報とは",
        );
        let f = features(&facts, &[wiki]);
        assert_eq!(f[0][8], 0.0, "an encyclopedia does not answer the weather");
        assert_eq!(f[0][19], 1.0, "it is off the shape");
    }

    #[test]
    fn host_named_reads_the_site_name_not_a_list() {
        let facts = QueryFacts::new("how to set up a wireguard vpn server");
        assert_eq!(
            host_named(&facts, "https://www.wireguard.com/quickstart/"),
            1.0
        );
        assert_eq!(
            host_named(&facts, "https://tech-insider.org/wireguard/"),
            0.0
        );
        // A word of four or more letters names a longer label; the TLD is not a
        // label, and a stop word names nothing.
        let pg = QueryFacts::new("postgres index only scan");
        assert_eq!(host_named(&pg, "https://www.postgresql.org/docs/x"), 1.0);
        let org = QueryFacts::new("org chart software");
        assert_eq!(host_named(&org, "https://example.org/"), 0.0);
        let parts = QueryFacts::new("git commit message");
        assert_eq!(
            host_named(&parts, "https://git-scm.com/docs/git-commit"),
            1.0
        );
    }

    #[test]
    fn the_vote_is_a_feature_so_the_model_can_keep_the_fusion() {
        let facts = QueryFacts::new("tokio");
        let mut a = m("https://a.example/1", "tokio", "");
        a.score = 2.0;
        let mut b = m("https://b.example/2", "tokio", "");
        b.score = 1.0;
        let f = features(&facts, &[a, b]);
        assert_eq!(f[0][0], 1.0);
        assert_eq!(f[1][0], 0.5);
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
        let x = features(&facts, &items)[0];
        let total: f64 = contribution(&w, &x).iter().sum();
        assert!((total - score(&w, &x)).abs() < 1e-9);
    }
}
