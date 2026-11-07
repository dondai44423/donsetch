//! Quantities inside a `must_contain` phrase, matched by value.
//!
//! An agent checks a fact the way it read it: "€15M", "EUR 15 million",
//! "15 million euros" and "€15,000,000" are one claim, and a page states it in
//! whichever of them its writer chose. A phrase quantity that carries a
//! currency, a magnitude or a percent sign (or a number large enough to be
//! written with separators) becomes a pattern for every common spelling of the
//! same value. Everything else in the phrase, and any number that is only a
//! number ("COVID-19", "2024"), is matched as written.
//!
//! False positives are bounded by construction: the currency, when the phrase
//! names one, must stand next to the number on the page; a single-letter
//! magnitude (`15m`) counts only beside a currency, where it cannot be metres;
//! and a number never matches the head of a longer one (`€15` is not `€15,000`).

use std::sync::LazyLock;

/// One quantity found in a phrase: its byte span and the page pattern for it.
pub(super) struct Found {
    pub start: usize,
    pub end: usize,
    pub pattern: String,
}

struct Currency {
    /// Spellings, as regex alternatives (case-insensitive).
    spellings: &'static [&'static str],
}

const CURRENCIES: &[Currency] = &[
    Currency {
        spellings: &["€", r"eur\b", r"euros?\b"],
    },
    Currency {
        spellings: &[r"us\$", r"\$", r"usd\b", r"dollars?\b"],
    },
    Currency {
        spellings: &["£", r"gbp\b", r"pounds?(?:\s+sterling)?\b"],
    },
    Currency {
        spellings: &["¥", r"jpy\b", r"yen\b"],
    },
    Currency {
        spellings: &["¥", r"cny\b", r"rmb\b", r"yuan\b"],
    },
    Currency {
        spellings: &["₹", r"inr\b", r"rupees?\b", r"rs\.?"],
    },
    Currency {
        spellings: &[r"chf\b", r"swiss\s+francs?\b"],
    },
];

/// What a written currency token is, by the spelling that matched it.
fn currency_of(token: &str) -> Option<&'static Currency> {
    let token = token.trim().to_lowercase();
    let is = |set: &[&str]| set.contains(&token.as_str());
    let index = if is(&["€", "eur", "euro", "euros"]) {
        0
    } else if is(&["$", "us$", "usd", "dollar", "dollars"]) {
        1
    } else if is(&["£", "gbp", "pound", "pounds"]) {
        2
    } else if is(&["jpy", "yen"]) {
        3
    } else if is(&["¥", "cny", "rmb", "yuan"]) {
        // `¥` is both; the yuan reading keeps `¥` and adds its codes.
        4
    } else if is(&["₹", "inr", "rupee", "rupees"]) {
        5
    } else if is(&["chf"]) {
        6
    } else {
        return None;
    };
    Some(&CURRENCIES[index])
}

/// Magnitudes: their multiplier, the words for them, and the one-letter forms
/// that count only beside a currency.
const MAGNITUDES: &[(f64, &[&str], &str)] = &[
    (1e3, &["thousand", "thousands"], "k"),
    (1e6, &["million", "millions", "mln", "mn", "mio"], "m"),
    (1e9, &["billion", "billions", "bln", "bn", "mrd", "md"], "b"),
    (1e12, &["trillion", "trillions", "tn"], "t"),
];

fn magnitude_of(token: &str, with_currency: bool) -> Option<f64> {
    let token = token.trim().to_lowercase();
    MAGNITUDES.iter().find_map(|(times, words, letter)| {
        (words.contains(&token.as_str()) || (with_currency && token == *letter)).then_some(*times)
    })
}

/// A quantity as a phrase writes it.
static IN_PHRASE: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(
        r"(?xi)
        (?P<pre> € | us\$ | \$ | £ | ¥ | ₹ | \b(?:eur|usd|gbp|jpy|cny|rmb|inr|chf)\b )?
        \s*
        \b(?P<num> \d{1,3}(?:[,.\u{a0}\u{202f}'][0-9]{3})+(?:[.,][0-9]+)? | [0-9]+(?:[.,][0-9]+)? )
        (?: \s* (?P<mag> thousands? | millions? | billions? | trillions? | mln | mn | mio | bln | bn | mrd | md | tn | [kmbt] ) \b )?
        (?: \s* (?P<post> % | percent\b | per\s+cent\b | pct\b | euros?\b | dollars?\b | pounds?\b | yen\b | yuan\b | rupees?\b | € | \$ | £ | ¥ | ₹ | \b(?:eur|usd|gbp|jpy|cny|rmb|inr|chf)\b ) )?",
    )
    .expect("amount pattern")
});

/// The value of a number as written, with `,` `.` and spaces as either
/// thousands or decimal separators: both present, the last one is the decimal
/// point; one kind repeated is thousands; one occurrence before exactly three
/// digits is thousands; otherwise a decimal point.
fn number_value(raw: &str) -> Option<f64> {
    let separators: Vec<(usize, char)> = raw
        .char_indices()
        .filter(|(_, c)| !c.is_ascii_digit())
        .collect();
    let decimal_at = match separators.as_slice() {
        [] => None,
        seps => {
            let kinds: std::collections::BTreeSet<char> = seps.iter().map(|(_, c)| *c).collect();
            let (last_at, last) = *seps.last()?;
            let point = matches!(last, '.' | ',');
            if kinds.len() > 1 {
                point.then_some(last_at)
            } else if seps.len() > 1 {
                None
            } else {
                let digits_after = raw[last_at + last.len_utf8()..].len();
                (point && digits_after != 3).then_some(last_at)
            }
        }
    };
    let mut plain = String::new();
    for (at, c) in raw.char_indices() {
        if c.is_ascii_digit() {
            plain.push(c);
        } else if Some(at) == decimal_at {
            plain.push('.');
        }
    }
    plain.parse().ok()
}

/// Every spelling of the number `x` (at most three decimals): digits grouped by
/// threes with any common separator or none, and a decimal part with either
/// point and optional trailing zeros.
fn number_pattern(x: f64) -> String {
    let fixed = format!("{x:.3}");
    let (int, frac) = fixed.split_once('.').unwrap_or((&fixed, ""));
    let frac = frac.trim_end_matches('0');
    let mut groups: Vec<&str> = Vec::new();
    let mut end = int.len();
    while end > 3 {
        groups.push(&int[end - 3..end]);
        end -= 3;
    }
    groups.push(&int[..end]);
    groups.reverse();
    let mut out = groups[0].to_string();
    for group in &groups[1..] {
        out.push_str(r"[,.\s\u{a0}\u{202f}']?");
        out.push_str(group);
    }
    if frac.is_empty() {
        out.push_str(r"(?:[.,]0+)?");
    } else {
        out.push_str(r"[.,]");
        out.push_str(frac);
        out.push_str("0*");
    }
    out
}

/// The value as the page may write it: each magnitude it is a round number
/// of, with that magnitude's words.
fn value_pattern(value: f64, with_currency: bool, percent: bool) -> String {
    let mut forms: Vec<String> = Vec::new();
    if value < 1e15 {
        forms.push(number_pattern(value));
    }
    if !percent {
        for (times, words, letter) in MAGNITUDES {
            let x = value / times;
            let thousandths = x * 1000.0;
            if x < 0.1 || (thousandths - thousandths.round()).abs() > 1e-6 * thousandths.max(1.0) {
                continue;
            }
            let mut names: Vec<String> = words.iter().map(|word| word.to_string()).collect();
            if with_currency {
                names.push(letter.to_string());
            }
            forms.push(format!(
                r"{}\s*(?:{})\b",
                number_pattern(x),
                names.join("|")
            ));
        }
    }
    // Longest spellings first, so a magnitude is taken whole.
    forms.reverse();
    format!("(?:{})", forms.join("|"))
}

/// The quantities in `phrase` that are matched by value, in order.
pub(super) fn amounts(phrase: &str) -> Vec<Found> {
    let mut found = Vec::new();
    for caps in IN_PHRASE.captures_iter(phrase) {
        let whole = caps.get(0).expect("match");
        let num = caps.name("num").expect("number").as_str();
        let pre = caps.name("pre").map(|m| m.as_str());
        let post = caps.name("post").map(|m| m.as_str());
        let percent = post.is_some_and(|post| {
            let post = post.to_lowercase();
            post == "%" || post.starts_with("per") || post == "pct"
        });
        let currency = pre
            .and_then(currency_of)
            .or_else(|| post.filter(|_| !percent).and_then(currency_of));
        let magnitude = caps
            .name("mag")
            .map(|m| magnitude_of(m.as_str(), currency.is_some()));
        // A one-letter magnitude with no currency is a unit (`15m`), not a
        // quantity: the phrase is matched as written.
        if magnitude == Some(None) {
            continue;
        }
        let magnitude = magnitude.flatten();
        let Some(number) = number_value(num) else {
            continue;
        };
        let grouped = num.contains([',', '.', '\u{a0}', '\u{202f}', '\''])
            && number_value(num).is_some_and(|value| value >= 1000.0);
        let long = num.chars().all(|c| c.is_ascii_digit()) && num.len() >= 5;
        if currency.is_none() && magnitude.is_none() && !percent && !grouped && !long {
            continue;
        }
        let value = number * magnitude.unwrap_or(1.0);
        let mut pattern = value_pattern(value, currency.is_some(), percent);
        if percent {
            pattern.push_str(r"\s*(?:%|percent\b|per\s+cent\b|pct\b)");
        }
        // The span the pattern stands for: the currency and percent sign are
        // inside it only when the phrase wrote them there.
        let start = if pre.is_some() {
            whole.start()
        } else {
            caps.name("num").expect("number").start()
        };
        let end = match (caps.name("post"), percent || currency.is_some()) {
            (Some(post), true) => post.end(),
            _ => caps
                .name("mag")
                .map_or(caps.name("num").expect("number").end(), |m| m.end()),
        };
        let pattern = match currency {
            Some(currency) => {
                let names = currency.spellings.join("|");
                format!(r"(?:(?:{names})\s*{pattern}|{pattern}\s*(?:{names}))")
            }
            None => pattern,
        };
        // Never the head or tail of a longer number.
        found.push(Found {
            start,
            end,
            pattern: format!(r"(?:^|[^\d.,]){pattern}(?:$|[^\d.,]|[.,](?:$|[^\d]))"),
        });
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    fn matches(term: &str, page: &str) -> bool {
        let found = amounts(term);
        assert_eq!(found.len(), 1, "{term}: one amount");
        regex::RegexBuilder::new(&found[0].pattern)
            .case_insensitive(true)
            .build()
            .expect("pattern")
            .is_match(page)
    }

    #[test]
    fn one_value_in_every_spelling_a_page_uses() {
        for page in [
            "a grant of EUR 15 million",
            "raised €15M in May",
            "raised €15m",
            "15 million euros",
            "€15,000,000 was raised",
            "€ 15.000.000",
            "EUR 15 Mio.",
            "€15.0 million",
            "15M€",
        ] {
            assert!(matches("€15M", page), "€15M vs {page:?}");
        }
        for page in [
            "$15M",
            "€150M",
            "€1.5M",
            "€15,000",
            "€115 million",
            "15m long",
        ] {
            assert!(!matches("€15M", page), "€15M must not match {page:?}");
        }
    }

    #[test]
    fn decimals_and_magnitudes_agree() {
        assert!(matches("$1.5bn", "revenue of US$1,500 million"));
        assert!(matches("$1.5bn", "1.5 billion dollars"));
        assert!(matches("1.5 billion", "1,5 Mrd"));
        assert!(!matches("$1.5bn", "$1.6 billion"));
        assert!(matches("12,000", "12 000 attendees"));
        assert!(matches("12000", "12,000 attendees"));
        assert!(matches("£3k", "£3,000"));
    }

    #[test]
    fn percents_in_words_and_signs() {
        assert!(matches("15%", "grew 15 percent"));
        assert!(matches("15 per cent", "up 15%"));
        assert!(matches("2.5%", "2,5 %"));
        assert!(!matches("15%", "115%"));
        assert!(!matches("15%", "1.5%"));
    }

    #[test]
    fn plain_numbers_are_left_as_written() {
        for term in [
            "COVID-19",
            "version 2.3",
            "in 2024",
            "15m tall",
            "chapter 12",
        ] {
            assert!(amounts(term).is_empty(), "{term}");
        }
    }

    #[test]
    fn numbers_read_as_writers_write_them() {
        assert_eq!(number_value("15,000,000"), Some(15e6));
        assert_eq!(number_value("15.000.000"), Some(15e6));
        assert_eq!(number_value("1.5"), Some(1.5));
        assert_eq!(number_value("1,5"), Some(1.5));
        assert_eq!(number_value("1,500"), Some(1500.0));
        assert_eq!(number_value("1.234,56"), Some(1234.56));
        assert_eq!(number_value("1,234.56"), Some(1234.56));
        assert_eq!(number_value("12\u{a0}000"), Some(12000.0));
    }
}
