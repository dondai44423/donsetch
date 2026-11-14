//! Language and script.
//!
//! A script is not a language and a language is not a country: Cyrillic is
//! Ukrainian, Russian, Belarusian, Bulgarian and more; Devanagari is Nepali,
//! Hindi, Marathi and more. The search path never infers a language or a
//! market from the characters in a query. `lang` is an explicit caller input;
//! the only default is the English language for Latin-script queries. A region
//! is passed only when the caller writes it in the tag.
//!
//! [`native_script`] is the bench's script-level label check, not a product
//! market decision: it can say "this title is Cyrillic", never "this is
//! Ukrainian rather than Russian".

/// An explicit language tag: a 2–3 letter language, optionally a region.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Lang {
    code: String,
    region: Option<String>,
}

impl Lang {
    /// Parse a BCP-47-shaped language tag: `en`, `uk`, `ne`, `pt-BR`.
    ///
    /// Script subtags (`zh-Hant`) are refused here: the sources this tag
    /// reaches take a language and, optionally, a market, not a script.
    pub fn parse(raw: &str) -> Result<Self, String> {
        let raw = raw.trim();
        let bad = || "lang must be a language tag like en, uk, ne or pt-BR".to_string();
        if raw.is_empty() {
            return Err(bad());
        }
        let mut parts = raw.split('-');
        let code = parts.next().unwrap_or_default();
        if !(2..=3).contains(&code.len()) || !code.bytes().all(|b| b.is_ascii_alphabetic()) {
            return Err(bad());
        }
        let region = match parts.next() {
            None => None,
            Some(region)
                if parts.next().is_none()
                    && ((region.len() == 2 && region.bytes().all(|b| b.is_ascii_alphabetic()))
                        || (region.len() == 3 && region.bytes().all(|b| b.is_ascii_digit()))) =>
            {
                Some(region.to_ascii_uppercase())
            }
            Some(_) => return Err(bad()),
        };
        Ok(Self {
            code: code.to_ascii_lowercase(),
            region,
        })
    }

    /// The English language without a market.
    pub fn en() -> Self {
        Self {
            code: "en".to_string(),
            region: None,
        }
    }

    /// The language subtag, lowercase.
    pub fn code(&self) -> &str {
        &self.code
    }

    /// The region subtag, uppercase, only when the caller wrote one.
    pub fn region(&self) -> Option<&str> {
        self.region.as_deref()
    }

    /// The normalized tag, for cache keys and debug output.
    pub fn tag(&self) -> String {
        match &self.region {
            Some(region) => format!("{}-{region}", self.code),
            None => self.code.clone(),
        }
    }

    /// DuckDuckGo's `kl` is a region-language market. It exists only when the
    /// caller stated a region; the language alone is not a market.
    pub fn kl(&self) -> Option<String> {
        self.region
            .as_ref()
            .map(|region| format!("{}-{}", region.to_ascii_lowercase(), self.code))
    }
}

/// The language the sources should use.
///
/// An explicit tag wins. Otherwise a Latin-script query gets the English
/// language default (no market); any other script gets no language and no
/// country. The script is never turned into a language or a market.
pub fn effective(query: &str, explicit: Option<&Lang>) -> Option<Lang> {
    explicit
        .cloned()
        .or_else(|| latin_query(query).then(Lang::en))
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

/// Whether the unspecified-language Latin default applies. Han-only text has
/// no inferred market, but must not therefore inherit an English one.
pub fn latin_query(query: &str) -> bool {
    query
        .chars()
        .all(|c| matches!(script(c), Script::Latin | Script::Other))
}

/// The scripts a language is written in, for the bench's label check. A
/// language that shares a script with others (Cyrillic, Devanagari) can only
/// be checked at script level; this is why the product does not use it as a
/// market or language decision.
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

/// Is this result's title (or, when the title has no letters, its snippet)
/// written in the script a bench row labels as `lang`?
///
/// This is a **bench label check**, never a search-path language decision: a
/// Cyrillic title satisfies `ru` and `uk` alike; a Devanagari title satisfies
/// `hi` and `ne` alike. The product no longer asks it to tell languages apart.
pub fn native_script(lang: &str, title: &str, snippet: &str) -> bool {
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
    fn a_language_tag_is_language_and_optional_region() {
        assert_eq!(Lang::parse("en").unwrap().tag(), "en");
        assert_eq!(Lang::parse("UK").unwrap().tag(), "uk");
        assert_eq!(Lang::parse("pt-br").unwrap().tag(), "pt-BR");
        assert_eq!(Lang::parse("ne").unwrap().region(), None);
        assert_eq!(Lang::parse("uk-UA").unwrap().kl().as_deref(), Some("ua-uk"));
        assert_eq!(Lang::parse("en").unwrap().kl(), None);
    }

    #[test]
    fn a_tag_that_is_not_a_language_is_refused() {
        for bad in ["", "e", "english", "en-USA", "zh-Hant", "en-", "-en", "12"] {
            assert!(Lang::parse(bad).is_err(), "{bad:?} should be refused");
        }
    }

    #[test]
    fn only_latin_queries_get_the_english_default() {
        assert_eq!(
            effective("rust tokio", None).map(|l| l.tag()),
            Some("en".into())
        );
        assert_eq!(effective("Привіт", None), None);
        assert_eq!(effective("नेपाली", None), None);
        assert_eq!(
            effective("Привіт", Some(&Lang::parse("uk").unwrap())).map(|l| l.tag()),
            Some("uk".into())
        );
        assert_eq!(
            effective("rust", Some(&Lang::parse("es").unwrap())).map(|l| l.tag()),
            Some("es".into())
        );
    }

    #[test]
    fn script_is_a_query_shape_signal() {
        assert!(!latin_query("東京 地震 速報"));
        assert!(!latin_query("docker 中国 教程"));
        assert!(latin_query("Rust ownership 2026"));
    }

    #[test]
    fn native_script_is_script_level_and_says_so() {
        assert!(native_script("ja", "東京都で震度5弱", ""));
        assert!(!native_script("ja", "Tokyo earthquake report", ""));
        assert!(native_script("ru", "новости москвы", ""));
        // A Cyrillic title satisfies both labels: the check cannot tell the
        // languages apart, and the product never uses it as one.
        assert!(native_script("ru", "новини Києва", ""));
        // A title with no letters leaves the snippet to say.
        assert!(native_script("ja", "123", "東京の天気"));
        assert!(!native_script("ja", "123", "Tokyo weather"));
    }
}
