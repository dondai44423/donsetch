//! `donsetch search --explain <query>`: why each result sits where it does.
//!
//! The query runs through the real search path (the cache is skipped, so there
//! is a merge to explain), then the merge's own inputs are ranked again with
//! [`rank::explain`]: the same pool, the same features, the same weights. Each
//! result prints its score and the features that moved it; with the learned
//! ranker installed, each weight's share of the score, which sums to the score.
//! The last line says whether the ranker's own order (scores, then the
//! diversity cap) is the order the search returned.

use super::ltr::{FEATURES, NAMES};
use super::rank::{self, Explained};
use super::{Searcher, doctor};

const USAGE: &str = "usage: donsetch search --explain <query> [--max-results N] [--json]";

/// True when `search`'s arguments ask for an explanation rather than a search.
pub fn wants(args: &[String]) -> bool {
    args.iter().any(|a| a == "--explain")
}

struct Args {
    query: String,
    max: usize,
    json: bool,
}

fn parse(args: &[String]) -> Result<Args, String> {
    let mut words = Vec::new();
    let mut max = 10;
    let mut json = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--explain" => {}
            "--json" => json = true,
            "--max-results" => {
                i += 1;
                max = args
                    .get(i)
                    .and_then(|v| v.parse::<usize>().ok())
                    .filter(|n| (1..=12).contains(n))
                    .ok_or_else(|| format!("--max-results is 1..=12\n{USAGE}"))?;
            }
            "-h" | "--help" => return Err(USAGE.to_string()),
            flag if flag.starts_with("--") => {
                return Err(format!("unknown argument {flag:?}\n{USAGE}"));
            }
            word => words.push(word.to_string()),
        }
        i += 1;
    }
    if words.is_empty() {
        return Err(USAGE.to_string());
    }
    Ok(Args {
        query: words.join(" "),
        max,
        json,
    })
}

/// Whether the search returned the ranker's order: every shown result is one
/// the explained merge placed, in the same relative order. The operator filters
/// only remove results, so they keep this true; a step that moves one breaks it.
fn reproduces(positions: &[Option<usize>]) -> bool {
    positions.iter().all(Option::is_some) && positions.windows(2).all(|w| w[0] < w[1])
}

/// The features worth printing for one result: with a model, the largest
/// shares of its score; without one, the non-zero features themselves.
fn reasons(e: &Explained) -> String {
    let mut pairs: Vec<(usize, f64)> = match &e.parts {
        Some(parts) => (0..FEATURES).map(|j| (j, parts[j])).collect(),
        None => (0..FEATURES).map(|j| (j, e.x[j])).collect(),
    };
    pairs.retain(|(_, v)| v.abs() >= 0.005);
    pairs.sort_by(|a, b| b.1.abs().total_cmp(&a.1.abs()));
    let mut reasons = pairs
        .iter()
        .take(6)
        .map(|(j, v)| match e.parts {
            Some(_) => format!("{:+.2} {}", v, NAMES[*j]),
            None => format!("{} {:.2}", NAMES[*j], v),
        })
        .collect::<Vec<_>>()
        .join("  ");
    if e.post_adjustment != 0.0 {
        reasons.push_str(&format!(
            "  {:+.3} page/feedback adjustment",
            e.post_adjustment
        ));
    }
    reasons
}

pub async fn run(args: &[String]) -> i32 {
    let a = match parse(args) {
        Ok(a) => a,
        Err(e) => {
            crate::diag!("{e}");
            return 2;
        }
    };
    let searcher: Searcher = match doctor::searcher() {
        Ok(s) => s,
        Err(e) => {
            crate::diag!("search: {e}");
            return 1;
        }
    };
    searcher.forget(&a.query);
    let outcome = match searcher.search(&a.query, a.max, None).await {
        Ok(o) => o,
        Err(e) => {
            crate::diag!("search: {e}");
            return 1;
        }
    };
    let Some(input) = searcher.last_merge() else {
        crate::diag!("search: nothing was merged for this query (a BYOK provider answered it)");
        return 1;
    };
    let explained = rank::explain(&input);
    // The answer the search returned is the truth; the explanation is matched
    // to it by page, so the operator filters that ran after the merge are
    // accounted for.
    let positions: Vec<Option<usize>> = outcome
        .results
        .iter()
        .map(|r| {
            let key = rank::norm_key(&r.url);
            explained.iter().position(|e| rank::norm_key(&e.url) == key)
        })
        .collect();
    let rows: Vec<&Explained> = positions.iter().flatten().map(|&i| &explained[i]).collect();
    let same = reproduces(&positions);
    let facts = super::query::QueryFacts::new(&input.query);
    let ranker = match rank::model() {
        Some(w) if w.ranks(facts.shape()) => format!("model {}", super::rank_weights::TRAINED_ON),
        Some(_) => format!(
            "fusion (the model does not rank {} queries)",
            facts.shape().label()
        ),
        None => "fusion".to_string(),
    };
    if a.json {
        let doc = serde_json::json!({
            "query": input.query,
            "shape": facts.shape().label(),
            "ranker": ranker,
            "features": NAMES,
            "order_reproduced": same,
            "results": rows,
        });
        println!("{}", serde_json::to_string_pretty(&doc).unwrap_or_default());
    } else {
        println!(
            "explain  {:?}  shape {}  ranker {ranker}",
            input.query,
            facts.shape().label()
        );
        for (i, e) in rows.iter().enumerate() {
            let tier = if e.tier == 0 { " record" } else { "" };
            println!("{:>2}  {:+.3}{tier}  {}", i + 1, e.score, e.url);
            println!("      {}", reasons(e));
        }
        println!(
            "order {}",
            if same {
                "reproduced: the ranker's order is the order shown"
            } else {
                "not reproduced: a step after the ranker moved results (search.quality_prior or search.outcome_feedback)"
            }
        );
    }
    i32::from(!same)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_order_is_reproduced_when_the_ranker_placed_it() {
        // A filter that removed the second result keeps the order.
        assert!(reproduces(&[Some(0), Some(2), Some(3)]));
        // A later step moved one up, or added one the ranker never placed.
        assert!(!reproduces(&[Some(1), Some(0)]));
        assert!(!reproduces(&[Some(0), None]));
    }

    #[test]
    fn the_query_is_every_word_that_is_not_a_flag() {
        let args: Vec<String> = ["--explain", "rust", "serde", "--json", "json"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let a = parse(&args).unwrap();
        assert_eq!(a.query, "rust serde json");
        assert!(a.json);
        assert!(parse(&["--explain".to_string()]).is_err());
    }
}
