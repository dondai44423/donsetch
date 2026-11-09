//! How a pricing page in one language writes a price and translates a string.
//!
//! The page's script formats a price with the browser's `Intl.NumberFormat`
//! for the page's minimal language id, in US dollars, with the currency as a
//! symbol for English and as the ISO code for every other language. Strings
//! pass through the page's two dictionaries. Both are reproduced here for the
//! locales aws.amazon.com serves; a page in any other locale is left as served.

use std::collections::HashMap;

use serde_json::Value;

/// Where the page's script reads its two dictionaries (its built-in defaults).
const I18N_STRINGS: &str = "https://i18n-string.us-west-2.prod.pricing.aws.a2z.com";
const G11N_ASSETS: &str = "https://b0.p.awsstatic.com/pricing-static-assets";

/// One page locale and the number format `Intl.NumberFormat` gives its
/// language for a currency amount.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Locale {
    /// The page's `data-locale`.
    tag: &'static str,
    /// The language the script formats and looks messages up in.
    lang: &'static str,
    /// The `dictionariesJson` file name.
    dict: &'static str,
    /// `$` before the number (English) or `USD` before or after it.
    currency: Currency,
    group: &'static str,
    decimal: &'static str,
    /// A four-digit integer part is grouped only when this is 1 (es, it: 2).
    min_grouping: usize,
}

#[derive(Debug, PartialEq, Eq)]
enum Currency {
    Symbol,
    CodeFirst,
    CodeLast,
}

const NBSP: &str = "\u{a0}";
const NNBSP: &str = "\u{202f}";

/// Measured with ICU 78 (`Intl.NumberFormat`, the formatter the page calls).
const LOCALES: &[Locale] = &[
    Locale {
        tag: "en-US",
        lang: "en",
        dict: "en",
        currency: Currency::Symbol,
        group: ",",
        decimal: ".",
        min_grouping: 1,
    },
    Locale {
        tag: "de-DE",
        lang: "de",
        dict: "de",
        currency: Currency::CodeLast,
        group: ".",
        decimal: ",",
        min_grouping: 1,
    },
    Locale {
        tag: "es-ES",
        lang: "es",
        dict: "es",
        currency: Currency::CodeLast,
        group: ".",
        decimal: ",",
        min_grouping: 2,
    },
    Locale {
        tag: "fr-FR",
        lang: "fr",
        dict: "fr",
        currency: Currency::CodeLast,
        group: NNBSP,
        decimal: ",",
        min_grouping: 1,
    },
    Locale {
        tag: "it-IT",
        lang: "it",
        dict: "it",
        currency: Currency::CodeLast,
        group: ".",
        decimal: ",",
        min_grouping: 2,
    },
    Locale {
        tag: "ja-JP",
        lang: "ja",
        dict: "ja",
        currency: Currency::CodeFirst,
        group: ",",
        decimal: ".",
        min_grouping: 1,
    },
    Locale {
        tag: "ko-KR",
        lang: "ko",
        dict: "ko",
        currency: Currency::CodeFirst,
        group: ",",
        decimal: ".",
        min_grouping: 1,
    },
    Locale {
        tag: "pt-BR",
        lang: "pt",
        dict: "pt",
        currency: Currency::CodeFirst,
        group: ".",
        decimal: ",",
        min_grouping: 1,
    },
    Locale {
        tag: "ru-RU",
        lang: "ru",
        dict: "ru",
        currency: Currency::CodeLast,
        group: NBSP,
        decimal: ",",
        min_grouping: 1,
    },
    Locale {
        tag: "zh-CN",
        lang: "zh",
        dict: "zh",
        currency: Currency::CodeFirst,
        group: ",",
        decimal: ".",
        min_grouping: 1,
    },
    Locale {
        tag: "zh-TW",
        lang: "zh-Hant",
        dict: "zh-Hant",
        currency: Currency::CodeFirst,
        group: ",",
        decimal: ".",
        min_grouping: 1,
    },
    Locale {
        tag: "id-ID",
        lang: "id",
        dict: "id",
        currency: Currency::CodeFirst,
        group: ".",
        decimal: ",",
        min_grouping: 1,
    },
    Locale {
        tag: "tr-TR",
        lang: "tr",
        dict: "tr",
        currency: Currency::CodeFirst,
        group: ".",
        decimal: ",",
        min_grouping: 1,
    },
    Locale {
        tag: "vi-VN",
        lang: "vi",
        dict: "vi",
        currency: Currency::CodeLast,
        group: ".",
        decimal: ",",
        min_grouping: 1,
    },
];

pub(super) fn locale(tag: &str) -> Option<&'static Locale> {
    LOCALES.iter().find(|l| l.tag == tag)
}

impl Locale {
    /// The two dictionary files the page loads, as `(name, url)`, in load order.
    pub(super) fn dictionaries(&self) -> [(String, String); 2] {
        let file = self.tag.replace('-', "_");
        [
            (
                format!("@i18n/{file}"),
                format!("{I18N_STRINGS}/{file}.json"),
            ),
            (
                format!("@g11n/{}", self.dict),
                format!("{G11N_ASSETS}/dictionariesJson/{}.json", self.dict),
            ),
        ]
    }

    /// The page's message bundle from the dictionaries that arrived; `None`
    /// when one did not, because a half-loaded bundle translates half a table.
    pub(super) fn messages(&self, data: &HashMap<String, Value>) -> Option<Messages> {
        let mut out = HashMap::new();
        for (name, _) in self.dictionaries() {
            let file = data.get(&name)?;
            // A file nested under the language is read there; a flat file is
            // the language's bundle itself.
            let bundle = match file.get(self.lang) {
                Some(Value::Object(inner)) => inner,
                _ => file.as_object()?,
            };
            for (key, value) in bundle {
                let text = match value {
                    Value::String(s) => s.clone(),
                    Value::Array(parts) => parts
                        .iter()
                        .map(|p| p.as_str().map(str::to_string))
                        .collect::<Option<Vec<_>>>()?
                        .join(" "),
                    _ => continue,
                };
                out.insert(key.clone(), text);
            }
        }
        Some(Messages(out))
    }

    /// A dollar amount with exactly `digits` places, as the page shows it.
    /// `int` and `frac` are the absolute value's digits.
    pub(super) fn money(&self, negative: bool, int: &str, frac: &str) -> String {
        let grouped = if int.len() >= 4 && int.len() >= 3 + self.min_grouping {
            let mut g = String::new();
            for (i, c) in int.chars().enumerate() {
                if i > 0 && (int.len() - i).is_multiple_of(3) {
                    g.push_str(self.group);
                }
                g.push(c);
            }
            g
        } else {
            int.to_string()
        };
        let number = if frac.is_empty() {
            grouped
        } else {
            format!("{grouped}{}{frac}", self.decimal)
        };
        let sign = if negative { "-" } else { "" };
        match self.currency {
            Currency::Symbol => format!("{sign}${number}"),
            Currency::CodeFirst => format!("{sign}USD{NBSP}{number}"),
            Currency::CodeLast => format!("{sign}{number}{NBSP}USD"),
        }
    }
}

/// The page's message bundle: a string the dictionaries hold is shown
/// translated, anything else as published.
pub(super) struct Messages(HashMap<String, String>);

impl Messages {
    pub(super) fn translate(&self, text: &str) -> String {
        // The script looks a string up with `/` written `[slash]` (a `/` would
        // split its lookup path) and formats the message with no arguments: a
        // `{placeholder}` prints `undefined`, as the page prints it.
        let Some(message) = self.0.get(&text.replace('/', "[slash]")) else {
            return text.to_string();
        };
        let mut out = String::with_capacity(message.len());
        let mut rest = message.as_str();
        while let Some(open) = rest.find('{') {
            let after = &rest[open + 1..];
            match after.find('}') {
                Some(close)
                    if close > 0
                        && after[..close]
                            .chars()
                            .all(|c| c.is_ascii_alphanumeric() || "-_. ".contains(c)) =>
                {
                    out.push_str(&rest[..open]);
                    out.push_str("undefined");
                    rest = &after[close + 1..];
                }
                _ => {
                    out.push_str(&rest[..=open]);
                    rest = after;
                }
            }
        }
        out.push_str(rest);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn show(tag: &str, raw: &str, digits: u32) -> String {
        super::super::format_price(raw, digits, locale(tag).unwrap()).unwrap()
    }

    /// Expected strings are `Intl.NumberFormat` output (ICU 78), with the
    /// options the page passes: USD, symbol for English, code otherwise, the
    /// same minimum and maximum fraction digits.
    #[test]
    fn each_locale_writes_a_price_as_the_browser_formats_it() {
        let cases: &[(&str, [&str; 4])] = &[
            ("en-US", ["$0.017", "$1,234.50", "$12,345,678.90", "-$1.50"]),
            (
                "de-DE",
                [
                    "0,017\u{a0}USD",
                    "1.234,50\u{a0}USD",
                    "12.345.678,90\u{a0}USD",
                    "-1,50\u{a0}USD",
                ],
            ),
            (
                "es-ES",
                [
                    "0,017\u{a0}USD",
                    "1234,50\u{a0}USD",
                    "12.345.678,90\u{a0}USD",
                    "-1,50\u{a0}USD",
                ],
            ),
            (
                "fr-FR",
                [
                    "0,017\u{a0}USD",
                    "1\u{202f}234,50\u{a0}USD",
                    "12\u{202f}345\u{202f}678,90\u{a0}USD",
                    "-1,50\u{a0}USD",
                ],
            ),
            (
                "it-IT",
                [
                    "0,017\u{a0}USD",
                    "1234,50\u{a0}USD",
                    "12.345.678,90\u{a0}USD",
                    "-1,50\u{a0}USD",
                ],
            ),
            (
                "ja-JP",
                [
                    "USD\u{a0}0.017",
                    "USD\u{a0}1,234.50",
                    "USD\u{a0}12,345,678.90",
                    "-USD\u{a0}1.50",
                ],
            ),
            (
                "ko-KR",
                [
                    "USD\u{a0}0.017",
                    "USD\u{a0}1,234.50",
                    "USD\u{a0}12,345,678.90",
                    "-USD\u{a0}1.50",
                ],
            ),
            (
                "pt-BR",
                [
                    "USD\u{a0}0,017",
                    "USD\u{a0}1.234,50",
                    "USD\u{a0}12.345.678,90",
                    "-USD\u{a0}1,50",
                ],
            ),
            (
                "ru-RU",
                [
                    "0,017\u{a0}USD",
                    "1\u{a0}234,50\u{a0}USD",
                    "12\u{a0}345\u{a0}678,90\u{a0}USD",
                    "-1,50\u{a0}USD",
                ],
            ),
            (
                "zh-CN",
                [
                    "USD\u{a0}0.017",
                    "USD\u{a0}1,234.50",
                    "USD\u{a0}12,345,678.90",
                    "-USD\u{a0}1.50",
                ],
            ),
            (
                "zh-TW",
                [
                    "USD\u{a0}0.017",
                    "USD\u{a0}1,234.50",
                    "USD\u{a0}12,345,678.90",
                    "-USD\u{a0}1.50",
                ],
            ),
            (
                "id-ID",
                [
                    "USD\u{a0}0,017",
                    "USD\u{a0}1.234,50",
                    "USD\u{a0}12.345.678,90",
                    "-USD\u{a0}1,50",
                ],
            ),
            (
                "tr-TR",
                [
                    "USD\u{a0}0,017",
                    "USD\u{a0}1.234,50",
                    "USD\u{a0}12.345.678,90",
                    "-USD\u{a0}1,50",
                ],
            ),
            (
                "vi-VN",
                [
                    "0,017\u{a0}USD",
                    "1.234,50\u{a0}USD",
                    "12.345.678,90\u{a0}USD",
                    "-1,50\u{a0}USD",
                ],
            ),
        ];
        for (tag, want) in cases {
            let got = [
                show(tag, "0.0170000000", 3),
                show(tag, "1234.5", 2),
                show(tag, "12345678.9", 2),
                show(tag, "-1.5", 2),
            ];
            assert_eq!(&got, want, "{tag}");
        }
        assert!(locale("en-GB").is_none(), "Intl writes US$ there: not read");
    }

    #[test]
    fn a_string_is_translated_only_when_the_dictionaries_hold_it() {
        let de = locale("de-DE").unwrap();
        let [(i18n, _), (g11n, _)] = de.dictionaries();
        assert_eq!(i18n, "@i18n/de_DE");
        assert_eq!(g11n, "@g11n/de");
        let data = HashMap::from([
            (
                i18n.clone(),
                serde_json::json!({"Up to 10 Gigabit": "Bis zu 10 Gigabit", "High": "Hoch",
                    "1[slash]4 GPU": "1/4 GPU (de)", "Count {n}": "Anzahl {n}"}),
            ),
            (
                g11n.clone(),
                serde_json::json!({"de": {"High": "Hoch (g11n)"}}),
            ),
        ]);
        let m = de.messages(&data).unwrap();
        assert_eq!(m.translate("Up to 10 Gigabit"), "Bis zu 10 Gigabit");
        assert_eq!(
            m.translate("High"),
            "Hoch (g11n)",
            "the later dictionary wins"
        );
        assert_eq!(m.translate("db.r6g.large"), "db.r6g.large");
        assert_eq!(m.translate("1/4 GPU"), "1/4 GPU (de)");
        assert_eq!(m.translate("Count {n}"), "Anzahl undefined");
        let missing = HashMap::from([(i18n, serde_json::json!({}))]);
        assert!(de.messages(&missing).is_none());
    }
}
