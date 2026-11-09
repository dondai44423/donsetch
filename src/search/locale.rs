//! Locale: what language a query is in, and which results are native to it.
//!
//! No model and no dictionary — the script is the signal. A query with kana is
//! Japanese, with hangul Korean, with Cyrillic Russian; Han alone is left
//! ambiguous (kanji-only Japanese and Chinese share the script);
//! the same test, applied to a result's title and snippet, says whether that
//! result is *native* to the query or a page from another language that happens
//! to share a word.
//!
//! The locale then travels to the sources that accept one (Bing's `setlang` and
//! `cc`, DuckDuckGo's `kl`, Yahoo's `vl`) and to Wikipedia's own API, which has
//! an edition per language.

/// A query's language, and the market its sources should answer from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Locale {
    pub lang: &'static str,
    pub region: &'static str,
}

impl Locale {
    /// DuckDuckGo's `kl` (market-language), e.g. `jp-jp`.
    pub fn kl(self) -> String {
        let market = match self.lang {
            "ja" => "jp",
            "zh" => "cn",
            "ko" => "kr",
            "ru" => "ru",
            "ar" => "xa",
            "he" => "il",
            "th" => "th",
            "hi" => "in",
            "el" => "gr",
            other => other,
        };
        format!("{market}-{}", self.lang)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Script {
    Latin,
    Han,
    Kana,
    Hangul,
    Cyrillic,
    Arabic,
    Hebrew,
    Thai,
    Devanagari,
    Greek,
    Other,
}

pub fn script(c: char) -> Script {
    let u = c as u32;
    match u {
        0x3040..=0x309F | 0x30A0..=0x30FF | 0x31F0..=0x31FF => Script::Kana,
        0x4E00..=0x9FFF | 0x3400..=0x4DBF | 0xF900..=0xFAFF => Script::Han,
        0xAC00..=0xD7AF | 0x1100..=0x11FF | 0x3130..=0x318F => Script::Hangul,
        0x0400..=0x052F => Script::Cyrillic,
        0x0600..=0x06FF | 0x0750..=0x077F | 0xFB50..=0xFDFF => Script::Arabic,
        0x0590..=0x05FF => Script::Hebrew,
        0x0E00..=0x0E7F => Script::Thai,
        0x0900..=0x097F => Script::Devanagari,
        0x0370..=0x03FF | 0x1F00..=0x1FFF => Script::Greek,
        0x0041..=0x024F | 0x1E00..=0x1EFF => Script::Latin,
        _ => Script::Other,
    }
}

/// The query's language, when it is not the default (Latin script).
pub fn detect(query: &str) -> Option<Locale> {
    let (mut kana, mut hangul) = (0usize, 0usize);
    let mut only = None;
    for c in query.chars() {
        match script(c) {
            Script::Han => {}
            Script::Kana => kana += 1,
            Script::Hangul => hangul += 1,
            Script::Latin => {}
            Script::Cyrillic => {
                only.get_or_insert(Locale {
                    lang: "ru",
                    region: "RU",
                });
            }
            Script::Arabic => {
                only.get_or_insert(Locale {
                    lang: "ar",
                    region: "EG",
                });
            }
            Script::Hebrew => {
                only.get_or_insert(Locale {
                    lang: "he",
                    region: "IL",
                });
            }
            Script::Thai => {
                only.get_or_insert(Locale {
                    lang: "th",
                    region: "TH",
                });
            }
            Script::Devanagari => {
                only.get_or_insert(Locale {
                    lang: "hi",
                    region: "IN",
                });
            }
            Script::Greek => {
                only.get_or_insert(Locale {
                    lang: "el",
                    region: "GR",
                });
            }
            Script::Other => {}
        }
    }
    if kana > 0 {
        return Some(Locale {
            lang: "ja",
            region: "JP",
        });
    }
    if hangul > 0 {
        return Some(Locale {
            lang: "ko",
            region: "KR",
        });
    }
    // Han alone is not a language: kanji-only Japanese and Chinese share the
    // script, and a dictionary would be needed to tell them apart. The query
    // stays without a locale (the sources answer by script), rather than being
    // guessed into the wrong market.
    only
}

/// The scripts a language is written in.
fn scripts_of(lang: &str) -> &'static [Script] {
    match lang {
        "ja" => &[Script::Kana, Script::Han],
        "zh" => &[Script::Han],
        "ko" => &[Script::Hangul],
        "ru" => &[Script::Cyrillic],
        "ar" => &[Script::Arabic],
        "he" => &[Script::Hebrew],
        "th" => &[Script::Thai],
        "hi" => &[Script::Devanagari],
        "el" => &[Script::Greek],
        _ => &[],
    }
}

fn has_letters(text: &str) -> bool {
    text.chars()
        .any(|c| script(c) != Script::Other && !c.is_ascii_digit())
}

fn in_script(lang: &str, text: &str) -> bool {
    let want = scripts_of(lang);
    text.chars().any(|c| want.contains(&script(c)))
}

/// Is this result written in the query's language?
///
/// The **title** decides: a Korean headline is native to a Korean query even
/// when the snippet quotes an English sentence. A title with no letters at all
/// (a number, a code) leaves the snippet to say. Script-level, not
/// language-level: a Japanese page whose title is all kanji and a Chinese page
/// are indistinguishable without a dictionary, and both are native to a
/// Han-script query. What this rejects is the real failure — an English page
/// answering a Japanese query.
pub fn native(lang: &str, title: &str, snippet: &str) -> bool {
    if in_script(lang, title) {
        return true;
    }
    if has_letters(title) {
        return false;
    }
    in_script(lang, snippet) || !has_letters(snippet)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_script_names_the_language() {
        assert_eq!(detect("東京の地震速報").map(|l| l.lang), Some("ja"));
        assert_eq!(detect("地震速報 について").map(|l| l.lang), Some("ja"));
        assert_eq!(detect("서울 날씨").map(|l| l.lang), Some("ko"));
        assert_eq!(detect("новости москва").map(|l| l.lang), Some("ru"));
        assert_eq!(detect("rust tokio"), None);
    }

    #[test]
    fn kanji_alone_stays_ambiguous() {
        // `地震速報` is Japanese in use but Han-only by script; guessing a
        // market from it would send the query to the wrong sources.
        assert_eq!(detect("地震速報"), None);
        assert_eq!(detect("東京 地震 速報"), None);
    }

    #[test]
    fn native_is_script_level_and_says_so() {
        assert!(native("ja", "東京都で震度5弱", ""));
        assert!(native("ja", "地震速報 東京", ""));
        assert!(!native("ja", "Tokyo earthquake report", ""));
        assert!(native("zh", "北京天气预报", ""));
        assert!(!native("zh", "Beijing weather", ""));
        assert!(native(
            "ko",
            "서울특별시, 서울시, 대한민국 시간별 날씨",
            "Hourly weather"
        ));
        assert!(!native("ko", "Seoul weather", ""));
        assert!(native("ru", "новости москвы", ""));
        assert!(!native("ru", "Moscow news", ""));
        // A title with no letters leaves the snippet to say.
        assert!(native("ja", "123", "東京の天気"));
        assert!(!native("ja", "123", "Tokyo weather"));
        assert!(native("ja", "123 456", ""));
    }

    #[test]
    fn the_market_code_follows_the_language() {
        assert_eq!(
            Locale {
                lang: "ja",
                region: "JP"
            }
            .kl(),
            "jp-ja"
        );
        assert_eq!(
            Locale {
                lang: "zh",
                region: "CN"
            }
            .kl(),
            "cn-zh"
        );
        assert_eq!(
            Locale {
                lang: "ko",
                region: "KR"
            }
            .kl(),
            "kr-ko"
        );
    }
}
