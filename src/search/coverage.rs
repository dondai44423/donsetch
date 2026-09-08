//! Query-entity coverage scoring.
//!
//! A final penalty layer applied after RRF + BM25 + consensus +
//! domain priors + cross-encoder rerank, before the sort.
//!
//! The problem it solves: BM25 tokenizes on non-alphanumeric
//! boundaries, which destroys two classes of query entities:
//!
//! - **Compound terms**: "B-tree" becomes `b` + `tree`. A result
//!   about "binary tree" matches `tree` and gets BM25 credit for
//!   a compound it doesn't contain. The cross-encoder might even
//!   agree : they're both tree data structures : but "binary tree"
//!   is not "B-tree".
//!
//! - **Version numbers**: "5.2" becomes `5` + `2`. A result about
//!   "5.5" matches `5` and gets credit. The version distinction
//!   is invisible to every layer below this one.
//!
//! This layer extracts these entities from the query, checks
//! whether each result's title + snippet + URL contains them
//! (in any normalized form), and applies a score multiplier:
//!
//! - **Anchor** (hyphenated compound) missing → 0.3× penalty.
//!   A result that doesn't contain "b-tree" / "b tree" / "btree"
//!   is almost certainly about a different topic.
//!
//! - **Specifier** (version like "5.2" or year like "2026"):
//!   if the query specifies a version and the result mentions a
//!   *different* version with the same major number, that's a
//!   version drift → 0.3× penalty. If no query version appears
//!   at all and a different version is present, same penalty.
//!   If ANY query version appears in the result, no penalty :
//!   handles "python 3.12 vs 3.11" comparison queries.
//!
//! The penalty is a multiplier on the existing score, so it
//! preserves relative ordering among equally-penalized results
//! and only reshuffles when some results match and others don't.

use super::rank::Merged;

/// Penalty for missing an anchor entity (hyphenated compound).
/// 0.3 = 70% score reduction. Strong enough to push truly
/// off-topic results below on-topic ones, but not a hard filter.
const ANCHOR_MISS_PENALTY: f64 = 0.3;

/// Penalty for version/year mismatch (different value, same
/// category). 0.3 = 70% score reduction. A wrong version is
/// a different entity : "GLM 5.5" is not "GLM 5.2", just as
/// "binary tree" is not "B-tree". Same severity as anchor miss.
const SPECIFIER_MISMATCH_PENALTY: f64 = 0.3;

#[derive(Debug, Clone)]
struct Entity {
    /// Lowercased match forms : any of these in the result text
    /// counts as a match.
    variants: Vec<String>,
}

/// Extract anchor and specifier entities from a query.
///
/// **Anchors** are hyphenated compounds containing at least one
/// alphabetic character: "B-tree" → variants ["b-tree", "b tree",
/// "btree"]. Pure numeric patterns like "1-2" are not anchors.
///
/// **Specifiers** are version numbers (\d{1,3}.\d{1,3}) and years
/// (20\d{2}). These are only penalized when a *different* value
/// of the same type appears in the result.
fn extract_entities(query: &str) -> (Vec<Entity>, Vec<Entity>) {
    let mut anchors = Vec::new();
    let mut specifiers = Vec::new();

    for token in query.split_whitespace() {
        let lower = token.to_lowercase();
        // Strip leading/trailing punctuation (keep hyphens and
        // dots for compound/version detection).
        let clean = lower.trim_matches(|c: char| !c.is_alphanumeric() && c != '-' && c != '.');

        if clean.is_empty() || clean.len() <= 1 {
            continue;
        }

        // Hyphenated compound → anchor (must have >= 1 alpha char
        // to avoid treating "1-2" as a compound)
        if clean.contains('-') && clean.chars().any(|c| c.is_alphabetic()) {
            let variants = vec![
                clean.to_string(),
                clean.replace('-', " "),
                clean.replace('-', ""),
            ];
            anchors.push(Entity { variants });
            continue;
        }

        // Version number (d{1,3}.d{1,3}) → specifier
        if let Some(v) = parse_version(clean) {
            specifiers.push(Entity { variants: vec![v] });
            continue;
        }

        // Year (20xx, exactly 4 digits) → specifier
        if is_year(clean) {
            specifiers.push(Entity {
                variants: vec![clean.to_string()],
            });
            continue;
        }
    }

    (anchors, specifiers)
}

/// Parse a version number: 1-3 digits, dot, 1-3 digits.
/// Returns the lowercased string if it matches, None otherwise.
fn parse_version(token: &str) -> Option<String> {
    let parts: Vec<&str> = token.split('.').collect();
    if parts.len() != 2 {
        return None;
    }
    let (major, minor) = (parts[0], parts[1]);
    if major.is_empty() || minor.is_empty() {
        return None;
    }
    if major.len() > 3 || minor.len() > 3 {
        return None;
    }
    if !major.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    if !minor.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    Some(token.to_lowercase())
}

/// Check if a token is a year: exactly "20" followed by 2 digits.
fn is_year(token: &str) -> bool {
    token.len() == 4 && token.starts_with("20") && token.chars().all(|c| c.is_ascii_digit())
}

/// Check if any variant of an entity appears in the text.
fn entity_covered(entity: &Entity, text_lower: &str) -> bool {
    entity
        .variants
        .iter()
        .any(|v| text_lower.contains(v.as_str()))
}

/// Find all version numbers (d{1,3}.d{1,3}) in text.
/// Used to detect version mismatches.
/// Boundary-aware version match (#164): `5.2` matches `5.2`, `5.2.1`
/// and `v5.2`, but not `15.2` (longer major continuing from the
/// left) or `5.20` (longer minor continuing to the right). A trailing
/// dot starts a patch segment of the same minor line and still counts.
/// Plain substring matching let wrong-version docs skip the
/// specifier-mismatch penalty entirely.
fn version_at_boundary(text: &str, version: &str) -> bool {
    text.match_indices(version).any(|(start, matched)| {
        let left_ok = start == 0
            || text[..start]
                .chars()
                .next_back()
                .is_some_and(|c| !c.is_ascii_digit() && c != '.');
        let end = start + matched.len();
        let right_ok = end >= text.len()
            || text[end..]
                .chars()
                .next()
                .is_some_and(|c| !c.is_ascii_digit());
        left_ok && right_ok
    })
}

fn find_versions(text: &str) -> Vec<String> {
    let bytes = text.as_bytes();
    let mut versions = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if !bytes[i].is_ascii_digit() {
            i += 1;
            continue;
        }
        let major_start = i;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
        }
        let major_len = i - major_start;
        if major_len == 0 || major_len > 3 {
            continue;
        }
        if i >= bytes.len() || bytes[i] != b'.' {
            continue;
        }
        i += 1;
        let minor_start = i;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
        }
        let minor_len = i - minor_start;
        if minor_len == 0 || minor_len > 3 {
            continue;
        }
        versions.push(text[major_start..i].to_string());
    }
    versions
}

/// Find all years (20xx, standalone 4-digit) in text.
fn find_years(text: &str) -> Vec<String> {
    let bytes = text.as_bytes();
    let mut years = Vec::new();
    let mut i = 0;
    while i + 4 <= bytes.len() {
        if bytes[i] == b'2'
            && bytes[i + 1] == b'0'
            && bytes[i + 2].is_ascii_digit()
            && bytes[i + 3].is_ascii_digit()
        {
            // Ensure it's not part of a longer number
            let prev_ok = i == 0 || !bytes[i - 1].is_ascii_digit();
            let next_ok = i + 4 >= bytes.len() || !bytes[i + 4].is_ascii_digit();
            if prev_ok && next_ok {
                years.push(text[i..i + 4].to_string());
            }
        }
        i += 1;
    }
    years
}

/// Apply coverage penalties to results. Modifies `.score` in-place.
///
/// No-op if the query has no anchor or specifier entities.
pub fn penalize(query: &str, results: &mut [Merged]) {
    let (anchors, specifiers) = extract_entities(query);
    if anchors.is_empty() && specifiers.is_empty() {
        return;
    }

    // Separate version specifiers from year specifiers.
    let query_versions: Vec<String> = specifiers
        .iter()
        .filter_map(|e| parse_version(&e.variants[0]))
        .collect();
    let query_years: Vec<String> = specifiers
        .iter()
        .filter_map(|e| {
            let v = &e.variants[0];
            if is_year(v) { Some(v.clone()) } else { None }
        })
        .collect();

    for r in results.iter_mut() {
        // Check title + snippet + URL for entity coverage.
        let text = format!("{} {} {}", r.title, r.snippet, r.url);
        let text_lower = text.to_lowercase();
        let mut penalty = 1.0;

        // Anchor penalty: missing a hyphenated compound = off-topic.
        for anchor in &anchors {
            if !entity_covered(anchor, &text_lower) {
                penalty *= ANCHOR_MISS_PENALTY;
            }
        }

        // Version specifier: only penalize if NO query version
        // appears in the result. Handles "python 3.12 vs 3.11"
        // : a result mentioning 3.11 matches one query version.
        let any_version_matches = query_versions
            .iter()
            .any(|v| version_at_boundary(&text_lower, v));
        if !any_version_matches {
            for qv in &query_versions {
                let query_major = qv.split('.').next().unwrap_or("");
                let found = find_versions(&text);
                let mismatch = found
                    .iter()
                    .any(|fv| fv != qv && fv.split('.').next().unwrap_or("") == query_major);
                if mismatch {
                    penalty *= SPECIFIER_MISMATCH_PENALTY;
                    break; // one mismatch is enough
                }
            }
        }

        // Year specifier: only penalize if NO query year appears
        // and a different year is present.
        let any_year_matches = query_years.iter().any(|y| text_lower.contains(y));
        if !any_year_matches && !query_years.is_empty() {
            let found = find_years(&text);
            let mismatch = found.iter().any(|fy| !query_years.contains(fy));
            if mismatch {
                penalty *= SPECIFIER_MISMATCH_PENALTY;
            }
        }

        r.score *= penalty;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn merged(title: &str, snippet: &str, url: &str, score: f64) -> Merged {
        Merged {
            title: title.into(),
            url: url.into(),
            snippet: snippet.into(),
            sources: vec![],
            score,
            published: None,
        }
    }

    // ── entity extraction ───────────────────────────────

    #[test]
    fn extracts_hyphenated_anchor() {
        let (anchors, _) = extract_entities("B-tree deletion in C");
        assert_eq!(anchors.len(), 1);
        assert_eq!(anchors[0].variants, vec!["b-tree", "b tree", "btree"]);
    }

    #[test]
    fn extracts_version_specifier() {
        let (_, specifiers) = extract_entities("GLM 5.2 model");
        assert!(specifiers.iter().any(|s| s.variants == vec!["5.2"]));
    }

    #[test]
    fn extracts_year_specifier() {
        let (_, specifiers) = extract_entities("best laptop 2026");
        assert!(specifiers.iter().any(|s| s.variants == vec!["2026"]));
    }

    #[test]
    fn no_entities_for_plain_query() {
        let (anchors, specifiers) = extract_entities("rust async programming");
        assert!(anchors.is_empty());
        assert!(specifiers.is_empty());
    }

    #[test]
    fn pure_numeric_hyphen_not_anchor() {
        let (anchors, _) = extract_entities("range 1-10 list");
        assert!(anchors.is_empty(), "1-10 is not a compound term");
    }

    #[test]
    fn multiple_anchors() {
        let (anchors, _) = extract_entities("cross-encoder vs dual-encoder comparison");
        assert_eq!(anchors.len(), 2);
    }

    #[test]
    fn multiple_versions() {
        let (_, specifiers) = extract_entities("python 3.12 vs 3.11 comparison");
        let versions: Vec<String> = specifiers
            .iter()
            .filter_map(|e| parse_version(&e.variants[0]))
            .collect();
        assert_eq!(versions.len(), 2);
        assert!(versions.contains(&"3.12".into()));
        assert!(versions.contains(&"3.11".into()));
    }

    #[test]
    fn version_with_3_digit_major() {
        assert_eq!(parse_version("100.5"), Some("100.5".into()));
        assert_eq!(parse_version("1000.5"), None);
    }

    #[test]
    fn long_decimal_not_version() {
        assert_eq!(parse_version("3.14159"), None, "pi is not a version");
    }

    // ── anchor penalty ─────────────────────────────────

    #[test]
    fn anchor_missing_penalizes() {
        let mut results = vec![
            merged(
                "B-tree deletion tutorial",
                "deleting from a b-tree",
                "https://b.com",
                1.0,
            ),
            merged(
                "Binary tree deletion",
                "delete nodes from binary tree",
                "https://bt.com",
                1.0,
            ),
        ];
        penalize("B-tree deletion in C", &mut results);
        // On-topic: no penalty
        assert!((results[0].score - 1.0).abs() < 1e-9);
        // Off-topic: 0.3x penalty
        assert!((results[1].score - 0.3).abs() < 1e-9);
    }

    #[test]
    fn anchor_variant_match_no_penalty() {
        // "b tree" (space variant) appears in the result
        let mut results = vec![merged(
            "B tree deletion",
            "deleting from a B tree data structure",
            "https://bt.com",
            1.0,
        )];
        penalize("B-tree deletion", &mut results);
        assert!(
            (results[0].score - 1.0).abs() < 1e-9,
            "space variant should match"
        );
    }

    #[test]
    fn anchor_concatenated_match_no_penalty() {
        // "btree" (concatenated variant) appears in the URL
        let mut results = vec![merged(
            "BTree library",
            "a btree implementation in C",
            "https://github.com/libbtree",
            1.0,
        )];
        penalize("B-tree implementation", &mut results);
        assert!(
            (results[0].score - 1.0).abs() < 1e-9,
            "concatenated variant in URL + snippet should match"
        );
    }

    #[test]
    fn anchor_in_url_matches() {
        // "b-tree" appears in the URL path
        let mut results = vec![merged(
            "Deletion tutorial",
            "how to delete keys from a tree structure",
            "https://example.com/b-tree/delete",
            1.0,
        )];
        penalize("B-tree deletion", &mut results);
        assert!(
            (results[0].score - 1.0).abs() < 1e-9,
            "anchor in URL should match"
        );
    }

    // ── version mismatch penalty ────────────────────────

    #[test]
    fn version_mismatch_penalizes() {
        let mut results = vec![
            merged(
                "GLM 5.2 benchmarks",
                "GLM-5.2 released June 2026",
                "https://z.ai",
                1.0,
            ),
            merged(
                "GLM 5.5 release date",
                "GLM 5.5 coming soon",
                "https://e.com",
                1.0,
            ),
        ];
        penalize("GLM 5.2 model", &mut results);
        // Correct version: no penalty
        assert!((results[0].score - 1.0).abs() < 1e-9);
        // Wrong version (5.5 vs 5.2, same major): 0.3x
        assert!((results[1].score - 0.3).abs() < 1e-9);
    }

    #[test]
    fn version_match_in_url_no_penalty() {
        let mut results = vec![merged(
            "Python tutorial",
            "learn python programming",
            "https://docs.python.org/3.12/",
            1.0,
        )];
        penalize("python 3.12 tutorial", &mut results);
        assert!(
            (results[0].score - 1.0).abs() < 1e-9,
            "version in URL should match"
        );
    }

    #[test]
    fn version_not_mentioned_no_penalty() {
        // Result doesn't mention any version → no mismatch
        let mut results = vec![merged(
            "GLM overview",
            "general-purpose language model overview",
            "https://glm.ai",
            1.0,
        )];
        penalize("GLM 5.2 capabilities", &mut results);
        assert!(
            (results[0].score - 1.0).abs() < 1e-9,
            "missing version (no other version present) should not penalize"
        );
    }

    #[test]
    fn version_vs_query_no_penalty_for_either() {
        // "python 3.12 vs 3.11" : result about 3.11 should NOT be
        // penalized because 3.11 is also a query version.
        let mut results = vec![merged(
            "Python 3.11 release",
            "Python 3.11 is the latest",
            "https://python.org",
            1.0,
        )];
        penalize("python 3.12 vs 3.11", &mut results);
        assert!(
            (results[0].score - 1.0).abs() < 1e-9,
            "result matching one of the vs-versions should not be penalized"
        );
    }

    #[test]
    fn version_different_major_no_mismatch() {
        // "5.2" in query, "3.8" in result : different major,
        // "3.8" is probably a statistic, not a version mismatch
        let mut results = vec![merged(
            "GLM model",
            "3.8 percent accuracy improvement",
            "https://glm.ai",
            1.0,
        )];
        penalize("GLM 5.2 accuracy", &mut results);
        assert!(
            (results[0].score - 1.0).abs() < 1e-9,
            "different major version should not trigger mismatch"
        );
    }

    // ── year mismatch penalty ───────────────────────────

    #[test]
    fn year_mismatch_penalizes() {
        let mut results = vec![
            merged(
                "Best laptops 2026",
                "top picks for 2026",
                "https://r.com",
                1.0,
            ),
            merged(
                "Best laptops 2024",
                "our 2024 recommendations",
                "https://old.com",
                1.0,
            ),
        ];
        penalize("best laptop 2026", &mut results);
        assert!((results[0].score - 1.0).abs() < 1e-9);
        assert!((results[1].score - 0.3).abs() < 1e-9);
    }

    #[test]
    fn year_match_no_penalty() {
        let mut results = vec![merged(
            "Window managers 2026",
            "published January 2026",
            "https://f.com",
            1.0,
        )];
        penalize("best linux window manager 2026", &mut results);
        assert!((results[0].score - 1.0).abs() < 1e-9);
    }

    #[test]
    fn year_not_mentioned_no_penalty() {
        let mut results = vec![merged(
            "Window manager guide",
            "comprehensive comparison of managers",
            "https://w.com",
            1.0,
        )];
        penalize("window manager 2026", &mut results);
        assert!(
            (results[0].score - 1.0).abs() < 1e-9,
            "no year in result should not penalize"
        );
    }

    // ── combined + edge cases ──────────────────────────

    #[test]
    fn combined_anchor_and_version() {
        let mut results = vec![
            merged(
                "GLM-5.2 benchmarks",
                "GLM-5.2 performance results",
                "https://z.ai/glm-5.2",
                1.0,
            ),
            merged(
                "GLM 5.5 overview",
                "GLM 5.5 model capabilities",
                "https://x.com",
                1.0,
            ),
        ];
        penalize("GLM-5.2 benchmarks", &mut results);
        // Result 0: has "glm-5.2" (anchor match) → no anchor penalty
        // But wait : "GLM-5.2" is an anchor, and the query is "GLM-5.2 benchmarks"
        // The anchor is "glm-5.2" with variants ["glm-5.2", "glm 5.2", "glm5.2"]
        // Result 0 title has "GLM-5.2" → matches "glm-5.2" → no penalty
        assert!((results[0].score - 1.0).abs() < 1e-9);
        // Result 1: has "glm 5.5" but not "glm-5.2" or "glm 5.2" or "glm5.2"
        // → anchor miss (0.3x). No version specifiers in this query
        // (the hyphenated compound absorbs the version).
        assert!(
            (results[1].score - 0.3).abs() < 1e-9,
            "wrong version in anchor → anchor miss penalty"
        );
    }

    #[test]
    fn empty_query_noop() {
        let mut results = vec![merged("test", "test", "https://t.com", 1.0)];
        penalize("", &mut results);
        assert!((results[0].score - 1.0).abs() < 1e-9);
    }

    #[test]
    fn short_query_noop() {
        let mut results = vec![merged("test", "test", "https://t.com", 1.0)];
        penalize("hi", &mut results);
        assert!((results[0].score - 1.0).abs() < 1e-9);
    }

    #[test]
    fn no_results_noop() {
        let mut results: Vec<Merged> = vec![];
        penalize("B-tree 5.2", &mut results);
        assert!(results.is_empty());
    }

    #[test]
    fn preserves_relative_order_when_all_penalized() {
        // If all results miss the anchor, they all get 0.3x.
        // Relative ordering must be preserved.
        let mut results = vec![
            merged("Binary tree A", "first binary tree", "https://a.com", 0.9),
            merged("Binary tree B", "second binary tree", "https://b.com", 0.5),
        ];
        penalize("B-tree deletion", &mut results);
        assert!(
            results[0].score > results[1].score,
            "relative order preserved: {} > {}",
            results[0].score,
            results[1].score
        );
        assert!((results[0].score - 0.27).abs() < 1e-9);
        assert!((results[1].score - 0.15).abs() < 1e-9);
    }

    #[test]
    fn url_version_match_prevents_mismatch_penalty() {
        // The version is only in the URL, not the title/snippet
        let mut results = vec![merged(
            "Tutorial",
            "learn the basics",
            "https://docs.python.org/3.12/tutorial/",
            1.0,
        )];
        penalize("python 3.12 tutorial", &mut results);
        assert!(
            (results[0].score - 1.0).abs() < 1e-9,
            "version in URL alone should prevent mismatch penalty"
        );
    }

    #[test]
    fn anchor_in_snippet_case_insensitive() {
        let mut results = vec![merged(
            "Deletion",
            "Delete operation in B-TREE data structures",
            "https://d.com",
            1.0,
        )];
        penalize("B-tree deletion", &mut results);
        assert!(
            (results[0].score - 1.0).abs() < 1e-9,
            "case-insensitive match in snippet"
        );
    }

    #[test]
    fn version_match_is_boundary_anchored() {
        // #164: "5.2" inside "5.20" is NOT a version match: a 5.20
        // page must take the same-major mismatch penalty, not a free
        // pass from substring containment.
        let mut results = vec![
            merged(
                "glorax 5.20 release notes",
                "what changed in 5.20",
                "https://ex.com/520",
                1.0,
            ),
            merged(
                "glorax 5.2 release notes",
                "what changed in 5.2",
                "https://ex.com/52",
                1.0,
            ),
        ];
        penalize("glorax 5.2", &mut results);
        assert!(
            results[0].score < results[1].score,
            "5.20 docs must rank below an exact 5.2 match"
        );
        assert!(
            results[0].score < 1.0,
            "5.20 page must take the mismatch penalty"
        );
    }

    #[test]
    fn version_boundary_allows_patch_and_prefix_forms() {
        // "5.2.1" and "v5.2" both satisfy a "5.2" request: no
        // penalty, no mismatch flag.
        let mut results = vec![
            merged(
                "glorax 5.2.1 changelog",
                "patch notes",
                "https://ex.com/a",
                1.0,
            ),
            merged("glorax v5.2 guide", "docs", "https://ex.com/b", 1.0),
        ];
        penalize("glorax 5.2", &mut results);
        assert!((results[0].score - 1.0).abs() < 1e-9, "5.2.1 satisfies 5.2");
        assert!((results[1].score - 1.0).abs() < 1e-9, "v5.2 satisfies 5.2");
    }

    #[test]
    fn version_boundary_longer_major_stays_neutral() {
        // "15.2" against a "5.2" request: no longer a false match,
        // but a different major is not a same-line mismatch either:
        // neutral, per the same-major-only penalty policy.
        let mut results = vec![merged(
            "glorax 15.2 release notes",
            "what changed in 15.2",
            "https://ex.com/152",
            1.0,
        )];
        penalize("glorax 5.2", &mut results);
        assert!(
            (results[0].score - 1.0).abs() < 1e-9,
            "different major: no penalty"
        );
    }
}
