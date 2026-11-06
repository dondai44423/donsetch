//! Content contracts.
//!
//! A fetch is only content if the served document satisfies its contract. The
//! checks are the frozen corpus's own predicates — title, minimum text, required
//! phrases, selector counts — so a pass here means the same thing it meant in the
//! baseline. Nothing in this module can be relaxed to make a fetch look better;
//! predicates are data, taken from the manifest.

use scraper::{Html, Selector};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct SelectorRule {
    pub selector: String,
    #[serde(default)]
    pub min_count: Option<usize>,
    #[serde(default)]
    pub min_text_chars: Option<usize>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct Contract {
    #[serde(default)]
    pub title_contains: Option<String>,
    #[serde(default)]
    pub text_contains: Option<Vec<String>>,
    #[serde(default)]
    pub min_text_chars: Option<usize>,
    #[serde(default)]
    pub selectors: Option<Vec<SelectorRule>>,
    #[serde(default)]
    pub allowed_origins: Option<Vec<String>>,
    /// A short page that asks for a human ("prove your humanity") is a wall,
    /// whatever its length: on for the generic contract, which has no phrase of
    /// the real page to look for.
    #[serde(default)]
    pub refuse_human_asks: bool,
}

/// The most visible text a page asking for a human carries: an interstitial
/// is a sentence or two plus a footer; an article that quotes such a phrase is
/// far longer.
pub const HUMAN_ASK_TEXT_MAX: usize = 1500;

#[derive(Clone, Debug, Serialize)]
pub struct Content {
    pub title: String,
    pub text_chars: usize,
    pub passed: bool,
    pub failures: Vec<String>,
}

impl Contract {
    /// Used when no manifest contract is supplied: enough text to be a real
    /// document, and no wall markers (the classifier handles those).
    pub fn generic() -> Self {
        Self {
            title_contains: None,
            text_contains: None,
            min_text_chars: Some(200),
            selectors: None,
            allowed_origins: None,
            refuse_human_asks: true,
        }
    }
}

/// Is this response a document a contract can read (HTML or XHTML)? A
/// declared media type decides; with none declared, a body that opens like
/// markup is one.
pub fn is_document(headers: &[(String, String)], body: &str) -> bool {
    match headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("content-type"))
        .map(|(_, value)| value.to_ascii_lowercase())
    {
        Some(media) => media.contains("html"),
        None => {
            let head = body.trim_start().get(..256).unwrap_or(body.trim_start());
            let head = head.to_ascii_lowercase();
            head.starts_with('<') && !head.starts_with("<?xml") || head.contains("<html")
        }
    }
}

/// A response's verdict under `contract`. A contract describes a document; a
/// resource that is not one (JSON, a feed, plain text, a PDF, an image) answers
/// with its own bytes, so when the caller named no contract (`generic`) it
/// verifies when it is not empty. The classifier still reads its status and
/// headers for walls.
pub fn verify_response(
    status: u16,
    headers: &[(String, String)],
    body: &str,
    final_url: &str,
    contract: &Contract,
    generic: bool,
) -> Content {
    if !(200..400).contains(&status) {
        return Content {
            title: String::new(),
            text_chars: 0,
            passed: false,
            failures: vec![format!("status {status}")],
        };
    }
    if generic && !is_document(headers, body) {
        let empty = body.trim().is_empty();
        return Content {
            title: String::new(),
            text_chars: body.chars().count(),
            passed: !empty,
            failures: if empty {
                vec!["empty body".to_string()]
            } else {
                Vec::new()
            },
        };
    }
    verify(body, final_url, contract)
}

/// Verify a document against a contract.
pub fn verify(body: &str, final_url: &str, contract: &Contract) -> Content {
    // Script and style bodies are not text a reader sees; leaving them in both
    // inflates the text volume and poisons phrase checks with JS source.
    let cleaned = strip_noise(body);
    let document = Html::parse_document(&cleaned);
    let title = document
        .select(&Selector::parse("title").expect("title selector"))
        .next()
        .map(|node| node.text().collect::<String>())
        .unwrap_or_default()
        .trim()
        .to_string();
    let text = normalize(&document.root_element().text().collect::<Vec<_>>().join(" "));
    let mut failures = Vec::new();

    if let Some(expected) = &contract.title_contains {
        if !title.to_lowercase().contains(&expected.to_lowercase()) {
            failures.push(format!("title missing {expected:?} (title={title:?})"));
        }
    }
    if let Some(phrases) = &contract.text_contains {
        let lower = text.to_lowercase();
        for phrase in phrases {
            if !lower.contains(&phrase.to_lowercase()) {
                failures.push(format!("text missing {phrase:?}"));
            }
        }
    }
    if let Some(minimum) = contract.min_text_chars {
        if text.chars().count() < minimum {
            failures.push(format!("text {} < {minimum}", text.chars().count()));
        }
    }
    if contract.refuse_human_asks && text.chars().count() <= HUMAN_ASK_TEXT_MAX {
        let seen = format!("{title} {text}").to_lowercase();
        if let Some(phrase) = crate::classify::ASKS_HUMAN
            .iter()
            .find(|phrase| seen.contains(*phrase))
        {
            failures.push(format!("asks for a human: {phrase:?}"));
        }
    }
    if let Some(rules) = &contract.selectors {
        for rule in rules {
            let Ok(selector) = Selector::parse(&rule.selector) else {
                failures.push(format!("bad selector {:?}", rule.selector));
                continue;
            };
            let matches: Vec<_> = document.select(&selector).collect();
            if let Some(minimum) = rule.min_count {
                if matches.len() < minimum {
                    failures.push(format!(
                        "selector {:?} matched {} < {minimum}",
                        rule.selector,
                        matches.len()
                    ));
                }
            }
            if let Some(minimum) = rule.min_text_chars {
                let chars: usize = matches
                    .iter()
                    .map(|node| node.text().collect::<String>().chars().count())
                    .sum();
                if chars < minimum {
                    failures.push(format!(
                        "selector {:?} text {chars} < {minimum}",
                        rule.selector
                    ));
                }
            }
        }
    }
    if let Some(origins) = &contract.allowed_origins {
        let origin = url::Url::parse(final_url)
            .ok()
            .map(|url| url.origin().ascii_serialization())
            .unwrap_or_default();
        if !origins.iter().any(|allowed| allowed == &origin) {
            failures.push(format!("final origin {origin} not allowed: {origins:?}"));
        }
    }

    Content {
        passed: failures.is_empty() && !text.is_empty(),
        title,
        text_chars: text.chars().count(),
        failures,
    }
}

/// Remove subtrees whose text is not content, and comments.
fn strip_noise(body: &str) -> String {
    let mut out = String::with_capacity(body.len());
    let lower = body.to_ascii_lowercase();
    let mut index = 0;
    while index < body.len() {
        let rest = &lower[index..];
        let skip = ["<script", "<style", "<noscript", "<template", "<!--"]
            .iter()
            .find_map(|open| {
                rest.strip_prefix(open).map(|_| {
                    let close = match *open {
                        "<!--" => "-->",
                        "<script" => "</script>",
                        "<style" => "</style>",
                        "<noscript" => "</noscript>",
                        _ => "</template>",
                    };
                    let end = rest.find(close).map(|at| index + at + close.len());
                    (index + open.len(), end)
                })
            });
        match skip {
            Some((_, Some(end))) => {
                out.push(' ');
                index = end;
            }
            // An unterminated block (a streamed script, a malformed comment) must
            // not swallow the rest of the document: skip the opener and go on.
            Some((after_open, None)) => {
                out.push(' ');
                index = after_open;
            }
            None => {
                let character = body[index..].chars().next().expect("in bounds");
                out.push(character);
                index += character.len_utf8();
            }
        }
    }
    out
}

fn normalize(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut space = false;
    for character in text.chars() {
        let mapped = match character {
            '\u{a0}' | '\u{200b}' | '\u{200c}' | '\u{200d}' | '\u{feff}' => ' ',
            other if other.is_whitespace() => ' ',
            other => other,
        };
        if mapped == ' ' {
            space = true;
            continue;
        }
        if space && !out.is_empty() {
            out.push(' ');
        }
        space = false;
        out.push(mapped);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_passing_contract_requires_every_check() {
        let body = "<html><head><title>Example Domain</title></head><body><p>documentation examples and more text here</p></body></html>";
        let contract: Contract = serde_json::from_str(
            r#"{"title_contains":"Example Domain","text_contains":["documentation examples"],
                "min_text_chars":20,"allowed_origins":["https://example.com"]}"#,
        )
        .unwrap();
        assert!(verify(body, "https://example.com/", &contract).passed);
        // One predicate failing fails the whole contract.
        assert!(!verify(body, "https://other.test/", &contract).passed);
    }

    #[test]
    fn a_short_page_asking_for_a_human_is_not_generic_content() {
        // Reddit's interstitial, as served to a client it doubts (2026-11-06).
        let interstitial = "<html><head><title>Reddit - Prove your humanity</title></head><body>\
            <h1>Prove your humanity</h1><p>We\u{2019}re committed to safety and security. But not \
            for bots. Complete the challenge below and let us know you\u{2019}re a real person.</p>\
            <footer>Reddit, Inc. \u{a9} 2026. All rights reserved. User Agreement Privacy Policy \
            Content Policy Help</footer></body></html>";
        let verdict = verify(
            interstitial,
            "https://www.reddit.com/",
            &Contract::generic(),
        );
        assert!(!verdict.passed, "{verdict:?}");
        assert!(verdict
            .failures
            .iter()
            .any(|failure| failure.starts_with("asks for a human")));
        // An article that quotes the phrase is long; it stays content.
        let article = format!(
            "<html><head><title>How sites prove your humanity</title></head><body><article>{}</article></body></html>",
            "Sites ask visitors to prove your humanity with puzzles, and this essay explains why. ".repeat(25)
        );
        assert!(verify(&article, "https://a.test/", &Contract::generic()).passed);
        // A caller's own contract decides for itself.
        let named = Contract {
            min_text_chars: Some(50),
            ..Contract::default()
        };
        assert!(verify(interstitial, "https://www.reddit.com/", &named).passed);
    }

    #[test]
    fn selector_counts_are_enforced() {
        let body = "<html><body><a href='/1'>one</a></body></html>";
        let contract: Contract =
            serde_json::from_str(r#"{"selectors":[{"selector":"a[href]","min_count":2}]}"#)
                .unwrap();
        let verified = verify(body, "https://example.com/", &contract);
        assert!(!verified.passed);
        assert!(verified.failures[0].contains("matched 1 < 2"));
    }
}
