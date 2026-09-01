//! Frontier relevance scoring: BM25-lite over anchor text + URL path
//! tokens, with real Okapi IDF when a site inventory is available.
//! The crawl spends its budget on pages that MATTER to the focus
//! query, not on the sitemap's order.
//!
//! Reuses the DonSift focus tokenizer: CJK bigrams, 12-language
//! stopwords, light stemming, accent folding all apply to crawl
//! scoring for free.
//!
//! IDF: the sitemap/map phase hands over the site's own URL
//! inventory; `FocusIdf` computes document frequencies over
//! path tokens, so a ubiquitous token like "docs" stops padding
//! the score of hundreds of pages while a distinctive one is
//! rare and pushes its pages up. No doc-length normalization
//! (B=0): the "documents" here are link text + path, dozens of
//! tokens at most, and length there is noise, documented
//! deliberately. Without an inventory (BFS mode, no sitemap),
//! scoring degenerates to the flat overlap weights of v3.4.3.

use std::collections::HashMap;

use crate::extract::focus;
use crate::extract::language;

/// Score one candidate URL against the focus query.
/// `anchor` = the link text where we found it ("" from sitemaps).
/// `path` = URL path. `focus` = None means no focus: score = 0
/// and the queue falls back to sitemap/depth order.
pub fn score_candidate(anchor: &str, path: &str, focus: Option<&str>) -> f64 {
    score_candidate_with_idf(anchor, path, focus, None)
}

/// Score one candidate URL against the focus query, with optional
/// Okapi IDF from the site inventory. `anchor` = the link text
/// where we found it ("" from sitemaps). `path` = URL path.
/// `focus` = None means no focus: score = 0 and the queue falls
/// back to sitemap/depth order. `idf` = None reproduces the
/// pre-IDF flat weights exactly (used when no inventory exists).
pub fn score_candidate_with_idf(
    anchor: &str,
    path: &str,
    focus: Option<&str>,
    idf: Option<&FocusIdf>,
) -> f64 {
    let Some(q) = focus else {
        return depth_prior(path);
    };
    let qlang = language::detect_from_text(q);
    let qtoks = focus::tokenize(q, &qlang);
    if qtoks.is_empty() {
        return depth_prior(path);
    }

    // Candidate text: anchor words are highest signal.
    let anchor_toks = focus::tokenize(anchor, &qlang);
    // Path tokens: split on /-_.
    let path_text = path.replace(['/', '-', '_', '.'], " ");
    let path_toks = focus::tokenize(&path_text, &qlang);

    let mut score = 0.0f64;
    for qt in &qtoks {
        // Anchor hit: strongest evidence.
        if anchor_toks.iter().any(|t| t == qt) {
            score += 3.0;
        }
        // Path hit: still meaningful.
        if path_toks.iter().any(|t| t == qt) {
            score += 1.5;
        }
    }
    // Normalize by query size so 1-term and 5-term queries are
    // comparable. Saturation: each token caps at its first hit.
    let base = score / qtoks.len().max(1) as f64;
    // BM25-lite IDF: when the crawl has a site inventory, weight
    // each query token by ln(1 + (N - df + 0.5) / (df + 0.5)).
    // Distinctive tokens multiply their hits; ubiquitous ones
    // shrink. Without an inventory the weights stay as-is.
    let weighted = if let Some(table) = idf {
        let mut s = 0.0f64;
        for qt in &qtoks {
            let mut term = 0.0f64;
            if anchor_toks.iter().any(|t| t == qt) {
                term += 3.0;
            }
            if path_toks.iter().any(|t| t == qt) {
                term += 1.5;
            }
            s += term * table.idf(qt);
        }
        s / qtoks.len().max(1) as f64
    } else {
        base
    };
    weighted + depth_prior(path)
}

/// Okapi BM25 inverse document frequency over a site's own URL
/// inventory (paths only: anchor text does not exist yet for
/// undiscovered pages). Built once at the end of the map phase
/// and shared immutably with the workers (Arc); the crawl can
/// add more paths later if it wants sharper estimates.
pub struct FocusIdf {
    docs: usize,
    df: HashMap<String, u32>,
}

impl FocusIdf {
    /// Build from an iterator of URL paths. Each path is one
    /// document; tokenize with the path-aware splitter and its
    /// own detected language.
    pub fn from_paths(paths: impl Iterator<Item = String>) -> Self {
        let mut df: HashMap<String, u32> = HashMap::new();
        let mut docs = 0usize;
        for p in paths {
            docs += 1;
            let lang = language::detect_from_text(&p);
            let toks = focus::tokenize(&p.replace(['/', '-', '_', '.'], " "), &lang);
            let mut seen: Vec<&String> = Vec::new();
            for t in &toks {
                if !seen.iter().any(|s| s == &t) {
                    seen.push(t);
                    *df.entry(t.clone()).or_insert(0) += 1;
                }
            }
        }
        Self { docs, df }
    }

    /// Okapi IDF: ln(1 + (N - df + 0.5) / (df + 0.5)). Unseen tokens
    /// get the full expression with df=0, i.e. the highest possible
    /// weight for this corpus: a rare query term IS the strongest
    /// signal available.
    pub fn idf(&self, term: &str) -> f64 {
        let df = self.df.get(term).copied().unwrap_or(0) as f64;
        let n = self.docs as f64;
        (1.0 + (n - df + 0.5) / (df + 0.5)).ln()
    }

    pub fn doc_count(&self) -> usize {
        self.docs
    }

    pub fn df(&self, term: &str) -> u32 {
        self.df.get(term).copied().unwrap_or(0)
    }
}

/// Path-depth prior: prefer shallower pages when relevance is
/// neutral. /docs/guide > /a/b/c/d/e.
fn depth_prior(path: &str) -> f64 {
    let segs = path.split('/').filter(|s| !s.is_empty()).count();
    -(segs as f64) * 0.15
}

/// Check if any focus query token appears in the anchor text or
/// URL path. Used as a hard gate for crawl outlinks: when a
/// focus query is set, links with zero token matches are NOT
/// enqueued. This prevents the crawler from following
/// navigation, footer, and sidebar links to off-topic sections.
/// Returns true for empty focus (no filter).
///
/// Compound token handling: query terms containing `_` or `-`
/// (e.g. `spawn_blocking`, `async-await`) are treated as compound
/// identifiers. A compound term matches if:
///   (a) its fragments appear as a contiguous, in-order run in the
///       path or anchor tokens (e.g. `spawn_blocking` right next to
///       each other in the path), OR
///   (b) ALL its fragments appear in the path/anchor tokens, in any
///       order.
/// (a) is token-boundary-safe by construction: a raw string
/// `contains` would match `auto-complete` inside `auto-completed`,
/// a different word once stemming strips the `-ed`.
/// (b) prevents the stemmed fragment `block` (from splitting
/// `spawn_blocking` → `spawn` + `block`) from matching unrelated
/// paths like `/ant-libp2p-allow-block-list/` where only `block`
/// appears without `spawn`.
pub fn focus_match(anchor: &str, path: &str, focus: &str) -> bool {
    let qlang = language::detect_from_text(focus);
    let qtoks = focus::tokenize(focus, &qlang);
    if qtoks.is_empty() {
        return true;
    }
    let anchor_toks = focus::tokenize(anchor, &qlang);
    let path_text = path.replace(['/', '-', '_', '.'], " ");
    let path_toks = focus::tokenize(&path_text, &qlang);
    let all_toks: Vec<&String> = anchor_toks.iter().chain(path_toks.iter()).collect();

    // Split query into whitespace-separated terms.
    for term in focus.split_whitespace() {
        let lower_term = term.to_lowercase();

        // Compound term (contains _ or -): check full form as a
        // contiguous token run, or ALL fragments as token matches
        // (any order).
        if lower_term.contains('_') || lower_term.contains('-') {
            let fragments = focus::tokenize(&lower_term, &qlang);
            // (a) Full compound form: fragments appear contiguous
            // and in order in the path or anchor tokens. A raw
            // string `contains` here would cross word boundaries
            // (e.g. "auto-complete" is a substring of
            // "auto-completed", a different word once "-ed" is
            // stripped). Token-subsequence matching can't.
            if !fragments.is_empty()
                && (contains_subsequence(&path_toks, &fragments)
                    || contains_subsequence(&anchor_toks, &fragments))
            {
                return true;
            }
            // (b) ALL fragments must match as tokens, any order.
            if !fragments.is_empty() && fragments.iter().all(|ft| all_toks.contains(&ft)) {
                return true;
            }
        } else {
            // Simple term: check if any token matches.
            let term_toks = focus::tokenize(&lower_term, &qlang);
            if term_toks.iter().any(|tt| all_toks.contains(&tt)) {
                return true;
            }
        }
    }

    false
}

/// True if `needle` appears as a contiguous, in-order run inside
/// `haystack`. Used for compound-term matching: token-boundary-safe
/// alternative to a raw string `contains`, which can match across
/// word boundaries.
fn contains_subsequence(haystack: &[String], needle: &[String]) -> bool {
    if needle.is_empty() || needle.len() > haystack.len() {
        return false;
    }
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn focus_anchor_beats_path() {
        let a = score_candidate("the migration guide", "/blog/x", Some("migration"));
        let b = score_candidate("click here", "/docs/migration", Some("migration"));
        assert!(a > b);
        assert!(b > 0.0);
    }

    #[test]
    fn no_focus_depth_prior_only() {
        let shallow = score_candidate("", "/a", None);
        let deep = score_candidate("", "/a/b/c/d", None);
        assert!(shallow > deep);
    }

    #[test]
    fn empty_query_falls_back() {
        assert_eq!(
            score_candidate("x", "/a", Some("")),
            0.0 + depth_prior("/a")
        );
    }

    #[test]
    fn idf_distinctive_token_beats_common_token() {
        // Issue #86 core regression: without IDF the query token
        // that matches every navigation page rates exactly like a
        // distinctive one. With the site inventory, the ubiquitous
        // token gets a low idf and the rare one pushes its page up.
        let corpus = FocusIdf::from_paths(
            [
                "/docs",
                "/docs/api",
                "/docs/guide",
                "/docs/reference",
                "/docs/tutorial",
            ]
            .iter()
            .map(|s| s.to_string()),
        );
        assert_eq!(corpus.doc_count(), 5);
        // Tokenize the query the way score_candidate will: stems,
        // stopwords, everything. Then find the corpus doc-freq for
        // BOTH terms so the assertion does not guess the stemmer.
        let qlang = language::detect_from_text("serializers docs");
        let qtoks = focus::tokenize("serializers docs", &qlang);
        assert_eq!(qtoks.len(), 2, "expected two query terms, got {qtoks:?}");
        // "docs" appears (after tokenization) in every corpus path.
        let docs_tok = &qtoks[1];
        assert!(
            corpus.df(docs_tok) >= 4,
            "{} is ubiquitous, got df {}",
            docs_tok,
            corpus.df(docs_tok)
        );
        let rare_tok = &qtoks[0];
        assert_eq!(
            corpus.df(rare_tok),
            0,
            "{} must be absent from the corpus",
            rare_tok
        );
        assert!(
            corpus.idf(rare_tok) > corpus.idf(docs_tok),
            "rare term must carry more idf weight"
        );
        // A candidate matching the rare term + a candidate matching
        // the common term: the rare one must rank higher.
        let common_path = "/docs/api";
        let rare_path = "/guide/serializers";
        let s_common =
            score_candidate_with_idf("", common_path, Some("serializers docs"), Some(&corpus));
        let s_rare =
            score_candidate_with_idf("", rare_path, Some("serializers docs"), Some(&corpus));
        assert!(
            s_rare > s_common,
            "rare-token page must outrank the docs-furniture page, got {s_rare} vs {s_common}"
        );
    }

    #[test]
    fn no_corpus_is_exactly_legacy_flat_weighting() {
        // The no-inventory path must remain byte-identical to the
        // pre-IDF behavior: anchor hit 3.0 + path hit 1.5, single
        // token, plus the depth prior.
        let got = score_candidate("migration guide", "/blog/x", Some("migration"));
        assert!(
            (got - 3.0 + 0.3).abs() < 1e-9,
            "legacy math drifted, got {got}"
        );

        // And the weighted form with an explicit None corpus must
        // equal the legacy form in every case.
        let corpus_none =
            score_candidate_with_idf("migration guide", "/blog/x", Some("migration"), None);
        assert_eq!(got, corpus_none, "None corpus must not change scoring");
    }

    #[test]
    fn cjk_focus_scores() {
        let s = score_candidate("什么是机器学习", "/some/article", Some("机器学习"));
        assert!(s > 0.0);
    }

    #[test]
    fn focus_match_basic() {
        assert!(focus_match(
            "spawn blocking tutorial",
            "/docs/async",
            "spawn_blocking"
        ));
        assert!(focus_match(
            "click here",
            "/tokio/task/spawn_blocking",
            "spawn_blocking"
        ));
        assert!(!focus_match("login", "/login", "spawn_blocking vs spawn"));
        assert!(!focus_match("pricing", "/pricing", "spawn_blocking"));
        assert!(focus_match("", "/tokio/spawn", "spawn_blocking vs spawn"));
        assert!(!focus_match("", "/tokio/bytes", "spawn_blocking vs spawn"));
    }

    #[test]
    fn focus_match_empty_is_passthrough() {
        assert!(focus_match("anything", "/any/path", ""));
    }

    #[test]
    fn focus_match_compound_no_false_positive() {
        // `block` from `spawn_blocking` must NOT match paths that
        // contain `block` but not `spawn` (e.g. unrelated crates
        // on docs.rs that happen to have "block" in the name).
        assert!(!focus_match(
            "",
            "/ant-libp2p-allow-block-list",
            "spawn_blocking vs spawn"
        ));
        assert!(!focus_match(
            "",
            "/async-blocking-bridger",
            "spawn_blocking vs spawn"
        ));
        assert!(!focus_match("", "/asm_block", "spawn_blocking vs spawn"));
    }

    #[test]
    fn focus_match_compound_substring_respects_word_boundaries() {
        // The compound-term "full form as substring" check (case a)
        // must not cross word boundaries: "auto-complete" is a raw
        // substring of "auto-completed" (auto-complete[d]), but the
        // page is about the past tense of a DIFFERENT stem
        // ("complet", once "-ed" is stripped), not the "complete"
        // feature.
        assert!(
            !focus_match("", "/features/auto-completed-suggestions", "auto-complete"),
            "auto-complete must not match auto-completed across a word boundary"
        );
        // Same shape with anchor text instead of path.
        assert!(
            !focus_match(
                "the field was auto-completed already",
                "/x",
                "auto-complete"
            ),
            "auto-complete must not match \"auto-completed\" in anchor text either"
        );
        // A real match must still work: the compound term appears
        // as its own word, not glued to surrounding letters.
        assert!(focus_match(
            "",
            "/docs/auto-complete-guide",
            "auto-complete"
        ));
    }

    #[test]
    fn focus_match_compound_in_path() {
        // Full compound form in path → match.
        assert!(focus_match(
            "",
            "/tokio/task/spawn_blocking",
            "spawn_blocking vs spawn"
        ));
    }

    #[test]
    fn focus_match_all_fragments() {
        // Both fragments `spawn` and `block` in path → match.
        assert!(focus_match(
            "spawn block tutorial",
            "/docs/spawn/block",
            "spawn_blocking vs spawn"
        ));
    }
}
