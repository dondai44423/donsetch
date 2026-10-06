//! BM25 block filter for focus=. Language-aware tokenization
//! with CJK bigram support, multi-language stopword lists, light
//! stemming, and accent folding. Hand-rolled: k1=1.2 b=0.75.
//! No hits → full content (never punish the agent for a bad
//! query).

use std::collections::{HashMap, HashSet};

use super::blocks::Block;
use super::language::{self, LanguageInfo};

const K1: f64 = 1.2;
const B: f64 = 0.75;

/// Max blocks for semantic rescue when the lexical query has no hits.
/// Larger pages keep the honest full-content fallback instead of paying
/// cross-encoder inference over a reference manual.
const SEMANTIC_MAX_BLOCKS: usize = 8;

/// Cross-encoder relevance threshold (sigmoid output [0,1]).
/// Blocks scoring above this are kept even if BM25 missed them.
/// 0.3 catches semantically relevant blocks while filtering
/// out navigation, boilerplate, and unrelated sections.
const XENC_THRESHOLD: f64 = 0.3;

// ── Stopwords ────────────────────────────────────────────────

const STOP_EN: &[&str] = &[
    "the", "a", "an", "and", "or", "of", "to", "in", "is", "are", "was", "were", "for", "on",
    "with", "as", "at", "by", "from", "it", "its", "this", "that", "be", "been", "has", "have",
    "had", "not", "but", "they", "their", "we", "you", "he", "she", "his", "her", "what", "which",
    "who", "how", "when", "do", "does", "did", "can", "could", "will", "would", "about", "than",
    "then", "so", "if", "no", "yes", "more", "most", "some", "any", "all", "each", "other", "such",
];

const STOP_ZH: &[&str] = &[
    "的",
    "了",
    "在",
    "是",
    "有",
    "和",
    "就",
    "不",
    "人",
    "都",
    "一",
    "也",
    "很",
    "到",
    "说",
    "要",
    "去",
    "会",
    "着",
    "没",
    "看",
    "好",
    "自己",
    "这",
    "那",
    "与",
    "及",
    "或",
    "但",
    "而",
    "因",
    "为",
    "把",
    "被",
    "让",
    "从",
    "向",
    "对",
    "跟",
    "给",
    "以",
    "之",
    "于",
    "所",
    "可",
    "能",
    "这个",
    "那个",
    "什么",
    "怎么",
    "为什么",
    "怎么",
    "些",
    "里",
    "上",
    "下",
    "中",
];

const STOP_JA: &[&str] = &[
    "は", "が", "を", "に", "で", "と", "から", "まで", "より", "へ", "の", "て", "た", "だ", "し",
    "も", "か", "な", "ん", "する", "いる", "ある", "これ", "それ", "あれ", "この", "その", "あの",
    "です", "ます", "こと", "もの", "たち", "たち", "さん", "よう", "たち",
];

const STOP_KO: &[&str] = &[
    "은",
    "는",
    "이",
    "가",
    "을",
    "를",
    "에",
    "에서",
    "의",
    "와",
    "과",
    "도",
    "로",
    "으로",
    "하다",
    "있다",
    "없다",
    "이",
    "그",
    "저",
    "우리",
    "너",
    "저희",
    "들",
    "등",
    "및",
    "또는",
    "그리고",
    "하지만",
    "때문",
];

const STOP_ES: &[&str] = &[
    "el", "la", "los", "las", "un", "una", "unos", "unas", "de", "del", "y", "o", "a", "en", "que",
    "es", "son", "por", "para", "con", "se", "su", "sus", "al", "lo", "no", "si", "mas", "pero",
    "como", "me", "te", "le", "les", "su", "mi", "tu", "eso", "esta", "este", "eso",
];

const STOP_FR: &[&str] = &[
    "le", "la", "les", "un", "une", "des", "du", "de", "et", "ou", "a", "en", "que", "qui", "est",
    "sont", "pour", "par", "avec", "se", "sa", "ses", "au", "ce", "ces", "ne", "pas", "mais",
    "comme", "mon", "ton", "son", "nous", "vous", "ils", "elles", "dans", "sur", "sous",
];

const STOP_DE: &[&str] = &[
    "der", "die", "das", "den", "dem", "des", "ein", "eine", "einer", "einen", "einem", "eines",
    "und", "oder", "in", "zu", "von", "mit", "ist", "sind", "auf", "nicht", "aber", "als", "auch",
    "wenn", "so", "den", "dem", "im", "am", "zum", "zur", "beim", "das", "daß",
];

const STOP_AR: &[&str] = &[
    "في",
    "من",
    "على",
    "إلى",
    "عن",
    "مع",
    "هذا",
    "هذه",
    "ذلك",
    "التي",
    "الذي",
    "الذين",
    "ما",
    "لا",
    "لم",
    "لن",
    "قد",
    "كان",
    "كانت",
    "هو",
    "هي",
    "هم",
    "هن",
    "إن",
    "أن",
    "أو",
    "ثم",
    "حتى",
    "كل",
    "بعض",
    "غير",
];

const STOP_HI: &[&str] = &[
    "और",
    "यह",
    "वह",
    "इस",
    "का",
    "की",
    "के",
    "में",
    "से",
    "को",
    "ने",
    "है",
    "हैं",
    "था",
    "थी",
    "थे",
    "कि",
    "जो",
    "भी",
    "नहीं",
    "पर",
    "या",
    "तो",
    "ही",
    "व",
    "एक",
    "लिए",
    "द्वारा",
    "साथ",
    "पर",
];

const STOP_NE: &[&str] = &[
    "र",
    "यो",
    "त्यो",
    "यस",
    "को",
    "का",
    "की",
    "मा",
    "बाट",
    "लाई",
    "छ",
    "छन्",
    "थियो",
    "थिइन्",
    "थिए",
    "वा",
    "तर",
    "पनि",
    "होइन",
    "गर्न",
    "भएको",
    "गर्दा",
    "यहाँ",
    "त्यहाँ",
    "कुनै",
    "सबै",
    "एक",
];

const STOP_PT: &[&str] = &[
    "o", "a", "os", "as", "um", "uma", "de", "do", "da", "dos", "das", "e", "ou", "em", "que", "é",
    "são", "para", "por", "com", "se", "seu", "sua", "no", "na", "nos", "nas", "não", "mas",
    "como", "mais", "este", "essa", "isso", "aquele", "aquela",
];

const STOP_RU: &[&str] = &[
    "и",
    "в",
    "во",
    "что",
    "на",
    "с",
    "со",
    "для",
    "из",
    "от",
    "до",
    "по",
    "о",
    "об",
    "при",
    "как",
    "не",
    "но",
    "или",
    "чтобы",
    "же",
    "ли",
    "бы",
    "был",
    "была",
    "было",
    "были",
    "это",
    "этот",
    "эта",
    "эти",
    "тот",
    "та",
    "он",
    "она",
    "они",
    "мы",
    "вы",
    "вы",
    "них",
    "ней",
    "него",
];

fn stopword_set(lang: &str) -> &'static [&'static str] {
    match lang {
        "zh" => STOP_ZH,
        "ja" => STOP_JA,
        "ko" => STOP_KO,
        "es" => STOP_ES,
        "fr" => STOP_FR,
        "de" => STOP_DE,
        "ar" => STOP_AR,
        "hi" => STOP_HI,
        "ne" => STOP_NE,
        "pt" => STOP_PT,
        "ru" => STOP_RU,
        _ => STOP_EN,
    }
}

/// Stopwords for the page's language AND English (agents
/// often query in English even for non-English pages).
fn is_stopword(token: &str, lang: &str) -> bool {
    if STOP_EN.contains(&token) {
        return true;
    }
    if lang != "en" {
        return stopword_set(lang).contains(&token);
    }
    false
}

// ── Accent folding (Latin only) ──────────────────────────────

/// Fold accented Latin → ASCII (café→cafe, naïve→naive).
/// Non-Latin scripts pass through unchanged. Helps
/// cross-lingual search; applied to tokens before stemming.
fn fold_ascii(c: char) -> char {
    let u = c as u32;
    // Latin-1 Supplement: common Western European.
    match u {
        0x00C0..=0x00C5 => 'A',          // À-Å
        0x00C8..=0x00CB => 'E',          // È-Ë
        0x00CC..=0x00CF => 'I',          // Ì-Ï
        0x00D2..=0x00D6 | 0x00D8 => 'O', // Ò-Ö, Ø
        0x00D9..=0x00DC => 'U',          // Ù-Ü
        0x00C7 => 'C',                   // Ç
        0x00D1 => 'N',                   // Ñ
        0x00E0..=0x00E5 => 'a',          // à-å
        0x00E8..=0x00EB => 'e',          // è-ë
        0x00EC..=0x00EF => 'i',          // ì-ï
        0x00F2..=0x00F6 | 0x00F8 => 'o', // ò-ö, ø
        0x00F9..=0x00FC => 'u',          // ù-ü
        0x00E7 => 'c',                   // ç
        0x00F1 => 'n',                   // ñ
        0x00DD | 0x00FD | 0x00FF => 'y', // Ý ý ÿ
        0x0178 => 'Y',                   // Ÿ
        // German ß → ss handled in fold_str (1→2 expansion).
        _ => {
            // Latin Extended-A: try common mappings.
            match u {
                0x0100..=0x0105 => {
                    if u.is_multiple_of(2) {
                        'a'
                    } else {
                        'A'
                    }
                } // Ā-ą (alternating)
                0x0106..=0x010D => {
                    if u.is_multiple_of(2) {
                        'c'
                    } else {
                        'C'
                    }
                } // Ć-č
                0x010E..=0x0113 => {
                    if u.is_multiple_of(2) {
                        'd'
                    } else {
                        'D'
                    }
                } // Ď-ď Ď-ď
                0x0114..=0x011B => {
                    if u.is_multiple_of(2) {
                        'e'
                    } else {
                        'E'
                    }
                } // Ĕ-ě
                0x011C..=0x0123 => {
                    if u.is_multiple_of(2) {
                        'g'
                    } else {
                        'G'
                    }
                } // Ĝ-ģ
                0x0124..=0x0127 => {
                    if u.is_multiple_of(2) {
                        'h'
                    } else {
                        'H'
                    }
                } // Ĥ-ħ
                0x0128..=0x0131 => {
                    if u.is_multiple_of(2) {
                        'i'
                    } else {
                        'I'
                    }
                } // Ĩ-ı
                0x0134..=0x0135 => {
                    if u == 0x0134 {
                        'J'
                    } else {
                        'j'
                    }
                } // Ĵ ĵ
                0x0136..=0x013B => {
                    let m = (u - 0x0136) % 2;
                    if u < 0x0138 {
                        if m == 0 { 'k' } else { 'K' }
                    } else {
                        if m == 0 { 'l' } else { 'L' }
                    }
                }
                0x013C..=0x0142 => {
                    if u.is_multiple_of(2) {
                        'l'
                    } else {
                        'L'
                    }
                }
                0x0143..=0x014B => {
                    if u.is_multiple_of(2) {
                        'n'
                    } else {
                        'N'
                    }
                }
                0x014C..=0x0151 => {
                    if u.is_multiple_of(2) {
                        'o'
                    } else {
                        'O'
                    }
                }
                0x0152 => 'O', // Œ
                0x0153 => 'o', // œ
                0x0154..=0x0159 => {
                    if u.is_multiple_of(2) {
                        'r'
                    } else {
                        'R'
                    }
                }
                0x015A..=0x0161 => {
                    if u.is_multiple_of(2) {
                        's'
                    } else {
                        'S'
                    }
                }
                0x0162..=0x0167 => {
                    if u.is_multiple_of(2) {
                        't'
                    } else {
                        'T'
                    }
                }
                0x0168..=0x0173 => {
                    if u.is_multiple_of(2) {
                        'u'
                    } else {
                        'U'
                    }
                }
                0x0174..=0x0175 => {
                    if u == 0x0174 {
                        'W'
                    } else {
                        'w'
                    }
                }
                0x0176..=0x0177 => {
                    if u == 0x0176 {
                        'Y'
                    } else {
                        'y'
                    }
                }
                0x0179..=0x017B => {
                    if u.is_multiple_of(2) {
                        'z'
                    } else {
                        'Z'
                    }
                }
                _ => c,
            }
        }
    }
}

/// Fold a string to ASCII for Latin scripts. Handles ß → ss.
fn fold_str(s: &str) -> String {
    // Quick check: already ASCII?
    if s.is_ascii() {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c == 'ß' {
            out.push_str("ss");
        } else if c as u32 <= 0x024F {
            out.push(fold_ascii(c));
        } else {
            out.push(c);
        }
    }
    out
}

// ── Light English stemmer ────────────────────────────────────

/// Simplified Porter-like stemmer. Covers the 80% of
/// English inflection: plurals, -ing, -ed, common suffixes.
/// Conservative: only strips when stem ≥ 3 chars. Better to
/// under-stem than over-stem (over-stemming merges
/// unrelated words, hurting BM25 precision).
fn stem_en(word: &str) -> String {
    let w = word;
    if w.len() < 4 {
        return w.to_string();
    }
    // -ness (before -ss guard, since "happiness" ends in "ss" but
    // the real suffix is "-ness")
    if w.ends_with("ness") && w.len() > 5 {
        return w[..w.len() - 4].to_string();
    }
    // -ment (before -ss guard for same reason)
    if w.ends_with("ment") && w.len() > 6 {
        return w[..w.len() - 4].to_string();
    }
    // -tion → -t
    if w.ends_with("tion") && w.len() > 5 {
        return format!("{}t", &w[..w.len() - 4]);
    }
    // -sses → -ss
    if w.ends_with("sses") {
        return w[..w.len() - 2].to_string();
    }
    // -ies → -i
    if w.ends_with("ies") && w.len() > 4 {
        return format!("{}i", &w[..w.len() - 3]);
    }
    // -ss → -ss (don't strip)
    if w.ends_with("ss") {
        return w.to_string();
    }
    // -ing
    if w.ends_with("ing") && w.len() > 4 {
        let stem = &w[..w.len() - 3];
        if stem.len() >= 3 {
            return stem_en_double(stem);
        }
    }
    // -edly → -e (looked, walked, talked)
    if w.ends_with("edly") && w.len() > 4 {
        let stem = &w[..w.len() - 4];
        if stem.len() >= 3 {
            return stem.to_string();
        }
    }
    // -ed
    if w.ends_with("ed") && w.len() > 3 {
        let stem = &w[..w.len() - 2];
        if stem.len() >= 3 {
            return stem_en_double(stem);
        }
    }
    // -ly
    if w.ends_with("ly") && w.len() > 3 {
        let stem = &w[..w.len() - 2];
        if stem.len() >= 3 {
            return stem.to_string();
        }
    }
    // -ers → -er
    if w.ends_with("ers") && w.len() > 4 {
        return w[..w.len() - 1].to_string();
    }
    // -er
    if w.ends_with("er") && w.len() > 4 {
        let stem = &w[..w.len() - 2];
        if stem.len() >= 3 {
            return stem.to_string();
        }
    }
    // -est
    if w.ends_with("est") && w.len() > 5 {
        return w[..w.len() - 3].to_string();
    }
    // -s (plural, after all other rules)
    if w.ends_with('s') && !w.ends_with("us") && !w.ends_with("ss") {
        let stem = &w[..w.len() - 1];
        if stem.len() >= 3 {
            return stem.to_string();
        }
    }
    w.to_string()
}

/// Handle Porter step 1b double-consonant: "running" → stem
/// "runn" → "run" (double n → single n). "hopping" → "hop"
/// → "hopping" → "hopp" → "hop". But "typing" → "typ" (no
/// double consonant, stays).
fn stem_en_double(stem: &str) -> String {
    let chars: Vec<char> = stem.chars().collect();
    let n = chars.len();
    if n >= 2 && chars[n - 1] == chars[n - 2] {
        let c = chars[n - 1];
        // Only double consonants (not vowels, not 'l'/'s'/'z'
        // which Porter treats specially : but we keep it simple).
        if !"aeioulsz".contains(c) {
            return chars[..n - 1].iter().collect();
        }
    }
    stem.to_string()
}

/// Light suffix stripping for Romance languages. Not a full
/// stemmer : just strips common inflectional endings to
/// improve cross-form matching. Conservative: stem ≥ 3 chars.
fn stem_romance(word: &str, lang: &str) -> String {
    let w = word;
    if w.len() < 5 {
        return w.to_string();
    }
    let suffixes: &[&str] = match lang {
        "es" => &[
            "amiento", "imiento", "acion", "aciones", "ando", "iendo", "ar", "er", "ir", "ado",
            "ido", "ando", "an", "en", "es", "os", "as", "a",
        ],
        "fr" => &[
            "ement", "ation", "ations", "issant", "er", "ir", "re", "ée", "ées", "ant", "ent",
            "ons", "ez", "s",
        ],
        "pt" => &[
            "amento", "imento", "ação", "ções", "ando", "endo", "indo", "ar", "er", "ir", "ado",
            "ido", "ão", "ões", "os", "as", "a",
        ],
        _ => &["tion", "ment", "ing", "ed", "es", "er"],
    };
    for suf in suffixes {
        if let Some(stem) = w.strip_suffix(suf).filter(|s| s.len() >= 3) {
            return stem.to_string();
        }
    }
    w.to_string()
}

/// Light suffix stripping for Germanic.
fn stem_german(word: &str) -> String {
    let w = word;
    if w.len() < 5 {
        return w.to_string();
    }
    for suf in &["en", "er", "es", "em", "e", "s", "n"] {
        if let Some(stem) = w.strip_suffix(suf).filter(|s| s.len() >= 3) {
            return stem.to_string();
        }
    }
    w.to_string()
}

fn stem(token: &str, lang: &str) -> String {
    match lang {
        "en" => stem_en(token),
        "es" | "fr" | "pt" => stem_romance(token, lang),
        "de" => stem_german(token),
        _ => token.to_string(), // CJK, Arabic, etc: no stemming
    }
}

// ── Tokenizer ────────────────────────────────────────────────

/// Tokenize text for BM25 indexing. Language-aware:
/// - CJK/Thai: character unigrams + bigrams
/// - Latin/Cyrillic/etc.: word-boundary split
/// - Accent folding for Latin
/// - Language-specific stopword removal
/// - Light stemming for English, Romance, German
///
/// The result is a list of normalized tokens ready for
/// BM25 scoring.
pub fn tokenize(text: &str, lang: &LanguageInfo) -> Vec<String> {
    if language::needs_char_tokenize(lang.script) {
        tokenize_cjk(text, &lang.code)
    } else {
        tokenize_word_split(text, &lang.code)
    }
}

/// Backwards-compatible tokenize for callers that don't
/// have language info (defaults to English).
#[allow(dead_code)]
pub fn tokenize_simple(text: &str) -> Vec<String> {
    tokenize_word_split(text, "en")
}

fn tokenize_word_split(text: &str, lang: &str) -> Vec<String> {
    let folded = if lang == "en" || lang == "es" || lang == "fr" || lang == "de" || lang == "pt" {
        fold_str(text)
    } else {
        text.to_string()
    };
    let folded = folded.to_lowercase();
    let mut tokens = Vec::new();
    for part in folded.split(|c: char| !c.is_alphanumeric()) {
        let t = part.trim();
        if t.is_empty() {
            continue;
        }
        // Latin: skip single chars (noise). CJK handled separately.
        if t.chars().count() < 2 {
            continue;
        }
        if is_stopword(t, lang) {
            continue;
        }
        let stemmed = stem(t, lang);
        if !stemmed.is_empty() && stemmed.chars().count() >= 2 {
            tokens.push(stemmed);
        }
    }
    tokens
}

fn tokenize_cjk(text: &str, lang: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut cjk_buf: Vec<char> = Vec::new();
    let mut word_buf = String::new();

    let flush_cjk = |buf: &mut Vec<char>, tokens: &mut Vec<String>| {
        if buf.is_empty() {
            return;
        }
        // Unigrams.
        for &c in buf.iter() {
            let s = c.to_string();
            if !is_stopword(&s, lang) {
                tokens.push(s);
            }
        }
        // Bigrams.
        for w in buf.windows(2) {
            let bg: String = w.iter().collect();
            let s1 = w[0].to_string();
            let s2 = w[1].to_string();
            if !is_stopword(&s1, lang) || !is_stopword(&s2, lang) {
                tokens.push(bg);
            }
        }
        buf.clear();
    };

    let flush_word = |buf: &mut String, tokens: &mut Vec<String>| {
        let t = buf.trim().to_lowercase();
        if t.chars().count() >= 2 && !is_stopword(&t, "en") {
            let stemmed = stem(&t, "en");
            if !stemmed.is_empty() && stemmed.chars().count() >= 2 {
                tokens.push(stemmed);
            }
        }
        buf.clear();
    };

    for c in text.chars() {
        let s = language::char_script(c);
        if language::needs_char_tokenize(s) {
            // CJK/Kana/Hangul/Thai char : flush any Latin word.
            flush_word(&mut word_buf, &mut tokens);
            cjk_buf.push(c);
        } else if c.is_alphanumeric() {
            // Latin/Cyrillic/etc : flush CJK buffer, collect word.
            flush_cjk(&mut cjk_buf, &mut tokens);
            word_buf.push(c);
        } else {
            // Whitespace/punctuation : flush both buffers.
            flush_cjk(&mut cjk_buf, &mut tokens);
            flush_word(&mut word_buf, &mut tokens);
        }
    }
    flush_cjk(&mut cjk_buf, &mut tokens);
    flush_word(&mut word_buf, &mut tokens);

    tokens
}

// ── BM25 filter ─────────────────────────────────────────────

/// Compute BM25 scores for each block against the query.
/// Returns a vector of scores (one per block, 0.0 = no match).
/// Used by both `filter` (BM25-only) and `filter_semantic`
/// (BM25 + cross-encoder union).
fn bm25_scores(blocks: &[Block], query: &str, lang: &LanguageInfo) -> Vec<f64> {
    let qterms = tokenize(query, lang);
    if qterms.is_empty() || blocks.is_empty() {
        return vec![0.0; blocks.len()];
    }

    // Document stats.
    let docs: Vec<Vec<String>> = blocks.iter().map(|b| tokenize(&b.text(), lang)).collect();
    let mut df: HashMap<&str, usize> = HashMap::new();
    for doc in &docs {
        let mut seen = std::collections::HashSet::new();
        for t in doc {
            if seen.insert(t.as_str()) {
                *df.entry(t.as_str()).or_insert(0) += 1;
            }
        }
    }
    let n = blocks.len() as f64;
    let avgdl = docs.iter().map(|d| d.len()).sum::<usize>() as f64 / n.max(1.0);

    // Score each block.
    let mut scores = vec![0.0f64; blocks.len()];
    for (i, doc) in docs.iter().enumerate() {
        let mut tf: HashMap<&str, usize> = HashMap::new();
        for t in doc {
            *tf.entry(t.as_str()).or_insert(0) += 1;
        }
        let dl = doc.len() as f64;
        for q in &qterms {
            let Some(&term_df) = df.get(q.as_str()) else {
                continue;
            };
            let idf = (1.0 + (n - term_df as f64 + 0.5) / (term_df as f64 + 0.5)).ln();
            let f = tf.get(q.as_str()).copied().unwrap_or(0) as f64;
            if f > 0.0 {
                scores[i] +=
                    idf * (f * (K1 + 1.0)) / (f + K1 * (1.0 - B + B * dl / avgdl.max(1.0)));
            }
        }
    }
    scores
}

/// BM25 block filter. Returns (kept blocks, fell_back).
/// fell_back = true when the query matched nothing and we
/// returned the full page : the CALLER must signal this,
/// or the agent mistakes full content for focus matches.
///
/// BM25-only version. Production code uses `filter_semantic`
/// (BM25 + cross-encoder union). This function is kept for
/// tests and as a pure-BM25 baseline.
#[allow(dead_code)]
pub fn filter<'a>(blocks: &'a [Block], query: &str, lang: &LanguageInfo) -> (Vec<&'a Block>, bool) {
    let qterms = tokenize(query, lang);
    if qterms.is_empty() || blocks.is_empty() {
        return (blocks.iter().collect(), false);
    }

    let scores = bm25_scores(blocks, query, lang);
    let max_score = scores.iter().cloned().fold(0.0f64, f64::max);
    if max_score <= 0.0 {
        return (blocks.iter().collect(), true); // no hits → full, SIGNAL it
    }

    // Keep blocks above a fraction of the max score, in doc order.
    let threshold = max_score * 0.15;
    let kept: Vec<&Block> = scores
        .iter()
        .enumerate()
        .filter(|(_, s)| **s >= threshold)
        .map(|(i, _)| &blocks[i])
        .collect();
    (kept, false)
}

// ── Section model ────────────────────────────────────────────

/// A section: heading block (if any) + body blocks until the
/// next heading at any level. The preamble (blocks before the
/// first heading) is a section with heading_idx = None.
struct Section {
    heading_idx: Option<usize>,
    body_idx: Vec<usize>,
}

/// Group blocks into sections. Each heading starts a new
/// section. Blocks before the first heading form a preamble
/// section with no heading.
fn build_sections(blocks: &[Block]) -> Vec<Section> {
    let mut sections: Vec<Section> = Vec::new();
    let mut current = Section {
        heading_idx: None,
        body_idx: Vec::new(),
    };
    for (i, b) in blocks.iter().enumerate() {
        match b {
            Block::Heading { .. } => {
                if !current.body_idx.is_empty() || current.heading_idx.is_some() {
                    sections.push(std::mem::replace(
                        &mut current,
                        Section {
                            heading_idx: Some(i),
                            body_idx: Vec::new(),
                        },
                    ));
                } else {
                    current.heading_idx = Some(i);
                }
            }
            _ => {
                current.body_idx.push(i);
            }
        }
    }
    if !current.body_idx.is_empty() || current.heading_idx.is_some() {
        sections.push(current);
    }
    sections
}

/// Section-aware block selection core.
///
/// For each section:
/// - Heading score > threshold (Section Gravity): keep the
///   heading AND every body block in the section. The heading
///   defines the topic; all content under it is relevant.
/// - Body score > threshold but heading does not (Inverse
///   Gravity): keep only the matching body blocks AND the
///   heading (the agent needs it for context).
/// - No match: drop the entire section.
///
/// Returns kept block indices (unsorted) and whether any
/// section was selected.
fn select_sections_core(blocks: &[Block], scores: &[f64], threshold: f64) -> Vec<usize> {
    let sections = build_sections(blocks);
    let mut kept: Vec<usize> = Vec::new();
    let mut kept_set: HashSet<usize> = HashSet::new();

    for s in &sections {
        let heading_score = s.heading_idx.map(|h| scores[h]).unwrap_or(0.0);
        let heading_matches = heading_score > threshold;

        if heading_matches {
            // Section Gravity: heading match pulls in entire section.
            if let Some(hi) = s.heading_idx
                && kept_set.insert(hi)
            {
                kept.push(hi);
            }
            for &bi in &s.body_idx {
                if kept_set.insert(bi) {
                    kept.push(bi);
                }
            }
        } else {
            // Check body blocks for matches.
            let mut body_matched = false;
            for &bi in &s.body_idx {
                if scores[bi] > threshold {
                    if kept_set.insert(bi) {
                        kept.push(bi);
                    }
                    body_matched = true;
                }
            }
            // Inverse Gravity: body match pulls in the heading.
            if body_matched
                && let Some(hi) = s.heading_idx
                && kept_set.insert(hi)
            {
                kept.push(hi);
            }
        }
    }

    kept
}

/// Breadcrumb Expansion: for each kept block, add all parent
/// heading blocks from its path that are not already kept.
/// This ensures the agent always sees structural context,
/// never an orphaned block.
fn expand_breadcrumbs(blocks: &[Block], kept: &mut Vec<usize>, kept_set: &mut HashSet<usize>) {
    let snapshot: Vec<usize> = kept.clone();
    for (hi, b) in blocks.iter().enumerate() {
        if let Block::Heading { text, .. } = b {
            if kept_set.contains(&hi) {
                continue;
            }
            let in_path = snapshot
                .iter()
                .any(|&bi| blocks[bi].path().iter().any(|p| p == text));
            if in_path {
                kept_set.insert(hi);
                kept.push(hi);
            }
        }
    }
}

/// Section-aware focus filter with Section Gravity.
///
/// Replaces flat BM25 block scoring with hierarchical section
/// scoring. Four mechanisms:
///
/// 1. **Section Gravity**: A heading match pulls in its entire
///    section. The heading defines the topic; all content under
///    it is relevant.
/// 2. **Inverse Gravity**: A body match pulls in its section
///    heading. The agent needs the heading for context.
/// 3. **Breadcrumb Expansion**: For each kept block, add all
///    parent heading blocks from its path. Never orphan a
///    block without structural context.
/// 4. **Cross-encoder Augmentation**: When the model is cached,
///    sections BM25 missed are checked by the cross-encoder.
///
/// The threshold for body-only matches is >0 (any keyword
/// appearance), not max*0.15. This is intentional: never cut
/// relevant info. Noise costs tokens; cut info is unrecoverable.
/// Run CPU-heavy inference off the async worker when we are inside a
/// multi-thread Tokio runtime (block_in_place), inline otherwise. This
/// mirrors the search-side offload: concurrent focus-extractions must
/// not park the worker pool on the shared ONNX session mutex.
fn offload_inference<R: Send + 'static>(work: impl FnOnce() -> R + Send + 'static) -> R {
    use tokio::runtime::RuntimeFlavor;
    if let Ok(handle) = tokio::runtime::Handle::try_current()
        && handle.runtime_flavor() == RuntimeFlavor::MultiThread
    {
        return tokio::task::block_in_place(work);
    }
    work()
}

fn xenc_scores(query: &str, docs: Vec<(String, String)>) -> Option<Vec<f64>> {
    let q = query.to_string();
    offload_inference(move || crate::search::rerank::cross_encoder_scores(&q, &docs))
}

pub fn filter_semantic<'a>(
    blocks: &'a [Block],
    query: &str,
    lang: &LanguageInfo,
) -> (Vec<&'a Block>, bool) {
    let qterms = tokenize(query, lang);
    if qterms.is_empty() || blocks.is_empty() {
        return (blocks.iter().collect(), false);
    }

    // Phase 1: BM25 scores for all blocks.
    let scores = bm25_scores(blocks, query, lang);
    let max_bm25 = scores.iter().cloned().fold(0.0f64, f64::max);

    if max_bm25 <= 0.0 {
        // BM25 found nothing. Try cross-encoder rescue.
        if blocks.len() <= SEMANTIC_MAX_BLOCKS && crate::search::rerank::is_model_cached() {
            let docs: Vec<(String, String)> =
                blocks.iter().map(|b| (b.text(), String::new())).collect();
            if let Some(xenc_scores) = xenc_scores(query, docs) {
                let xenc_max = xenc_scores.iter().cloned().fold(0.0f64, f64::max);
                if xenc_max >= XENC_THRESHOLD {
                    let mut kept = select_sections_core(blocks, &xenc_scores, XENC_THRESHOLD);
                    if !kept.is_empty() {
                        let mut kept_set: HashSet<usize> = kept.iter().copied().collect();
                        expand_breadcrumbs(blocks, &mut kept, &mut kept_set);
                        kept.sort_unstable();
                        return (kept.into_iter().map(|i| &blocks[i]).collect(), false);
                    }
                }
            }
        }
        return (blocks.iter().collect(), true); // fell_back
    }

    // Phase 2: Section-aware BM25 selection.
    let mut kept = select_sections_core(blocks, &scores, 0.0);

    if kept.is_empty() {
        return (blocks.iter().collect(), true);
    }

    let mut kept_set: HashSet<usize> = kept.iter().copied().collect();

    // Phase 3: Breadcrumb expansion.
    expand_breadcrumbs(blocks, &mut kept, &mut kept_set);

    // Sort by index to preserve document order.
    kept.sort_unstable();
    (kept.into_iter().map(|i| &blocks[i]).collect(), false)
}

// ── Code block fission ──────────────────────────────────────

/// Split large code blocks into sub-blocks for finer-grained
/// focus scoring. Called before focus when a focus query is
/// set. A 38k JSON schema becomes ~15 sub-blocks; focus can
/// then keep only the sub-blocks that match the query instead
/// of the entire monolith.
///
/// Returns the expanded block list. Blocks that are too small
/// or can't be split are passed through unchanged.
pub fn expand_code_blocks(blocks: Vec<Block>) -> Vec<Block> {
    let has_large_code = blocks.iter().any(|b| {
        if let Block::Code { code, .. } = b {
            code.len() > 2000
        } else {
            false
        }
    });
    if !has_large_code {
        return blocks;
    }

    let mut expanded = Vec::with_capacity(blocks.len());
    for b in blocks {
        if let Block::Code { code, lang, path } = &b
            && code.len() > 2000
        {
            let subs = split_code_block(code, lang.clone(), path.clone());
            if subs.len() > 1 {
                expanded.extend(subs);
                continue;
            }
        }
        expanded.push(b);
    }
    expanded
}

/// Split a code block into logical sub-blocks.
///
/// JSON: split on top-level keys (2-space indent `"key":`).
/// Other: split on blank-line-delimited sections.
/// Fallback: split at regular intervals.
///
/// Sub-blocks smaller than 200 chars are merged with neighbors.
fn split_code_block(code: &str, lang: Option<String>, path: Vec<String>) -> Vec<Block> {
    let lines: Vec<&str> = code.lines().collect();
    if lines.len() < 15 || code.len() < 2000 {
        return vec![Block::Code {
            lang,
            code: code.to_string(),
            path,
        }];
    }

    let is_json = code.trim_start().starts_with('{') || code.trim_start().starts_with('[');

    // Find split boundaries.
    let mut boundaries: Vec<usize> = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        if is_json {
            // Top-level key in pretty-printed JSON: 2-space indent,
            // starts with quote, not deeper indentation.
            if line.starts_with("  \"") && !line.starts_with("    ") {
                boundaries.push(i);
            }
        } else if line.trim().is_empty() && i > 0 && i < lines.len() - 1 {
            boundaries.push(i);
        }
    }

    if boundaries.len() < 2 {
        // Not enough natural boundaries. Fall back to interval split.
        let chunk = 40.max(lines.len() / 8);
        for i in (chunk..lines.len()).step_by(chunk) {
            boundaries.push(i);
        }
    }

    if boundaries.is_empty() {
        return vec![Block::Code {
            lang,
            code: code.to_string(),
            path,
        }];
    }

    // Build sub-blocks, merging small chunks (< 200 chars).
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    let mut start = 0;
    let min_chars = 200;

    for &b in &boundaries {
        if b <= start {
            continue;
        }
        let chunk_len: usize = lines[start..b].iter().map(|l| l.len() + 1).sum();
        if chunk_len >= min_chars {
            ranges.push((start, b));
            start = b;
        }
    }
    if start < lines.len() {
        ranges.push((start, lines.len()));
    }

    // Merge last chunk if too small.
    if ranges.len() > 1 {
        let last = ranges.len() - 1;
        let (ls, le) = ranges[last];
        let last_len: usize = lines[ls..le].iter().map(|l| l.len() + 1).sum();
        if last_len < min_chars {
            let (ps, _) = ranges[last - 1];
            ranges[last - 1] = (ps, le);
            ranges.pop();
        }
    }

    if ranges.len() <= 1 {
        return vec![Block::Code {
            lang,
            code: code.to_string(),
            path,
        }];
    }

    ranges
        .into_iter()
        .map(|(s, e)| Block::Code {
            lang: lang.clone(),
            code: lines[s..e].join("\n"),
            path: path.clone(),
        })
        .collect()
}

// ── Tests ────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::super::language::Script;
    use super::*;

    fn en() -> LanguageInfo {
        LanguageInfo {
            code: "en".to_string(),
            script: Script::Latin,
            scripts: vec![Script::Latin],
        }
    }

    fn zh() -> LanguageInfo {
        LanguageInfo {
            code: "zh".to_string(),
            script: Script::Han,
            scripts: vec![Script::Han],
        }
    }

    fn ja() -> LanguageInfo {
        LanguageInfo {
            code: "ja".to_string(),
            script: Script::Han,
            scripts: vec![Script::Han, Script::Kana],
        }
    }

    fn ko() -> LanguageInfo {
        LanguageInfo {
            code: "ko".to_string(),
            script: Script::Hangul,
            scripts: vec![Script::Hangul],
        }
    }

    // ── English tokenizer ──

    #[test]
    fn tokenize_en_basic() {
        let tokens = tokenize("The quick brown fox", &en());
        assert!(tokens.contains(&"quick".to_string()));
        assert!(tokens.contains(&"brown".to_string()));
        assert!(tokens.contains(&"fox".to_string()));
        assert!(!tokens.contains(&"the".to_string())); // stopword
    }

    #[test]
    fn tokenize_en_stemming() {
        let tokens = tokenize("running jumped quickly", &en());
        assert!(tokens.contains(&"run".to_string())); // running → run
        assert!(tokens.contains(&"jump".to_string())); // jumped → jump
        assert!(tokens.contains(&"quick".to_string())); // quickly → quick
    }

    #[test]
    fn tokenize_en_plural() {
        assert!(tokenize("cats", &en()).contains(&"cat".to_string()));
        assert!(tokenize("buses", &en()).contains(&"buse".to_string())); // buses → buse
        assert!(tokenize("berries", &en()).contains(&"berri".to_string())); // berries → berri
    }

    #[test]
    fn tokenize_en_min_length() {
        let tokens = tokenize("a I be to do", &en());
        assert!(tokens.is_empty()); // all stopwords or < 2 chars
    }

    // ── Chinese tokenizer ──

    #[test]
    fn tokenize_zh_unigrams_and_bigrams() {
        let tokens = tokenize("机器学习", &zh());
        // Unigrams.
        assert!(tokens.contains(&"机".to_string()));
        assert!(tokens.contains(&"器".to_string()));
        assert!(tokens.contains(&"学".to_string()));
        assert!(tokens.contains(&"习".to_string()));
        // Bigrams.
        assert!(tokens.contains(&"机器".to_string()));
        assert!(tokens.contains(&"器学".to_string()));
        assert!(tokens.contains(&"学习".to_string()));
    }

    #[test]
    fn tokenize_zh_stopwords() {
        let tokens = tokenize("的是在", &zh());
        assert!(!tokens.contains(&"的".to_string()));
        assert!(!tokens.contains(&"是".to_string()));
        assert!(!tokens.contains(&"在".to_string()));
    }

    #[test]
    fn tokenize_zh_mixed_latin() {
        let tokens = tokenize("Python编程语言", &zh());
        // CJK unigrams.
        assert!(tokens.contains(&"编".to_string()));
        assert!(tokens.contains(&"程".to_string()));
        // Latin word.
        assert!(tokens.contains(&"python".to_string()));
    }

    // ── Japanese tokenizer ──

    #[test]
    fn tokenize_ja_kana_and_kanji() {
        let tokens = tokenize("機械学習の分野", &ja());
        // Kanji unigrams.
        assert!(tokens.contains(&"機".to_string()));
        assert!(tokens.contains(&"械".to_string()));
        // "の" is a stopword : should be filtered.
        assert!(!tokens.contains(&"の".to_string()));
        // Non-stopword kana should be present.
        // "分" is a kanji (not kana), but let's test a non-stopword.
        assert!(tokens.contains(&"分".to_string()));
        // Bigrams.
        assert!(tokens.contains(&"機械".to_string()));
    }

    // ── Korean tokenizer ──

    #[test]
    fn tokenize_ko_unigrams_bigrams() {
        let tokens = tokenize("한국어", &ko());
        assert!(tokens.contains(&"한".to_string()));
        assert!(tokens.contains(&"국".to_string()));
        assert!(tokens.contains(&"어".to_string()));
        assert!(tokens.contains(&"한국".to_string()));
        assert!(tokens.contains(&"국어".to_string()));
    }

    // ── Accent folding ──

    #[test]
    fn fold_cafe() {
        assert_eq!(fold_str("café"), "cafe");
        assert_eq!(fold_str("naïve"), "naive");
        assert_eq!(fold_str("München"), "Munchen");
        assert_eq!(fold_str("résumé"), "resume");
    }

    #[test]
    fn fold_german_ss() {
        assert_eq!(fold_str("Straße"), "Strasse");
    }

    #[test]
    fn fold_preserves_non_latin() {
        assert_eq!(fold_str("日本語"), "日本語");
        assert_eq!(fold_str("العربية"), "العربية");
    }

    #[test]
    fn tokenize_accent_folding_en() {
        let tokens = tokenize("café résumé naïve", &en());
        assert!(tokens.contains(&"cafe".to_string()));
        assert!(tokens.contains(&"resume".to_string()));
        assert!(tokens.contains(&"naive".to_string()));
    }

    // ── Stemming ──

    #[test]
    fn stem_ing() {
        assert_eq!(stem_en("running"), "run");
        assert_eq!(stem_en("typing"), "typ");
        assert_eq!(stem_en("flying"), "fly");
    }

    #[test]
    fn stem_ed() {
        assert_eq!(stem_en("jumped"), "jump");
        assert_eq!(stem_en("walked"), "walk");
    }

    #[test]
    fn stem_ness_ment() {
        assert_eq!(stem_en("happiness"), "happi");
        assert_eq!(stem_en("development"), "develop");
    }

    #[test]
    fn stem_short_words() {
        assert_eq!(stem_en("cat"), "cat"); // too short to strip
        assert_eq!(stem_en("is"), "is"); // too short
    }

    #[test]
    fn stem_preserves_us() {
        assert_eq!(stem_en("status"), "status"); // -us not stripped
        assert_eq!(stem_en("genius"), "genius");
    }

    #[test]
    fn stem_german_basic() {
        assert_eq!(stem_german("machen"), "mach");
        assert_eq!(stem_german("Häuser"), "Häus"); // ä not folded here
        assert_eq!(stem_german("sagen"), "sag");
    }

    // ── BM25 filter ──

    use super::super::blocks::Block;

    fn para(text: &str) -> Block {
        Block::Para {
            md: text.to_string(),
            link_density: 0.0,
            path: vec![],
        }
    }

    #[test]
    fn bm25_basic_match() {
        let blocks = vec![
            para("Machine learning is a subset of artificial intelligence"),
            para("The weather is nice today"),
            para("Deep learning uses neural networks"),
        ];
        let (kept, fell_back) = filter(&blocks, "machine learning", &en());
        assert!(!fell_back);
        assert!(!kept.is_empty());
        // The block with "machine learning" should be kept.
        assert!(kept.iter().any(|b| b.text().contains("Machine learning")));
    }

    #[test]
    fn bm25_no_match_fell_back() {
        let blocks = vec![para("The weather is nice today"), para("I like pizza")];
        let (kept, fell_back) = filter(&blocks, "quantum physics", &en());
        assert!(fell_back);
        assert_eq!(kept.len(), 2); // all blocks returned
    }

    #[test]
    fn bm25_empty_query() {
        let blocks = vec![para("Some content")];
        let (kept, fell_back) = filter(&blocks, "", &en());
        assert!(!fell_back);
        assert_eq!(kept.len(), 1);
    }

    #[test]
    fn bm25_chinese_match() {
        let blocks = vec![
            para("机器学习是人工智能的一个分支领域"),
            para("今天天气很好"),
            para("深度学习使用神经网络"),
        ];
        let (kept, fell_back) = filter(&blocks, "机器学习", &zh());
        assert!(!fell_back);
        assert!(!kept.is_empty());
    }

    #[test]
    fn bm25_japanese_match() {
        let blocks = vec![
            para("機械学習は人工知能の一分野である"),
            para("今日はいい天気ですね"),
        ];
        let (kept, fell_back) = filter(&blocks, "機械学習", &ja());
        assert!(!fell_back);
        assert!(!kept.is_empty());
    }

    #[test]
    fn bm25_stemming_match() {
        let blocks = vec![para("The runner was running fast"), para("Cooking is fun")];
        // Query "run" should match "running" via stemming.
        let (kept, fell_back) = filter(&blocks, "run", &en());
        assert!(!fell_back);
        assert!(
            kept.iter()
                .any(|b| b.text().contains("runner") || b.text().contains("running"))
        );
    }

    #[test]
    fn bm25_accent_match() {
        let blocks = vec![para("Le café est délicieux"), para("The weather is nice")];
        // Query "cafe" should match "café" via accent folding.
        let (_kept, fell_back) = filter(&blocks, "cafe", &en());
        assert!(!fell_back);
    }

    // ── bm25_scores unit tests ──

    #[test]
    fn bm25_scores_positive_for_match() {
        let blocks = vec![
            para("Machine learning is a subset of artificial intelligence"),
            para("The weather is nice today"),
        ];
        let scores = bm25_scores(&blocks, "machine learning", &en());
        assert!(scores[0] > 0.0); // matching block
        assert_eq!(scores[1], 0.0); // non-matching block
    }

    #[test]
    fn bm25_scores_empty_query_zeros() {
        let blocks = vec![para("Some content")];
        let scores = bm25_scores(&blocks, "", &en());
        assert!(scores.iter().all(|s| *s == 0.0));
    }

    // ── filter_semantic tests ──
    // These tests assert properties that hold regardless of
    // whether the cross-encoder model is cached. filter_semantic
    // is a union (BM25 ∪ cross-encoder), so it always keeps at
    // least the BM25 matches.

    #[test]
    fn filter_semantic_empty_query() {
        let blocks = vec![para("Some content"), para("Other content")];
        let (kept, fell_back) = filter_semantic(&blocks, "", &en());
        assert!(!fell_back);
        assert_eq!(kept.len(), 2);
    }

    #[test]
    fn filter_semantic_keeps_bm25_matches() {
        let blocks = vec![
            para("Machine learning is a subset of artificial intelligence"),
            para("The weather is nice today"),
            para("Deep learning uses neural networks"),
        ];
        let (kept, fell_back) = filter_semantic(&blocks, "machine learning", &en());
        assert!(!fell_back);
        assert!(!kept.is_empty());
        assert!(kept.iter().any(|b| b.text().contains("Machine learning")));
    }

    #[test]
    fn filter_semantic_union_property() {
        // filter_semantic is a union: it keeps at least every
        // block that filter (BM25-only) keeps.
        let blocks = vec![
            para("Machine learning is a subset of artificial intelligence"),
            para("The weather is nice today"),
            para("Deep learning uses neural networks"),
        ];
        let (bm25_kept, _) = filter(&blocks, "machine learning", &en());
        let (sem_kept, _) = filter_semantic(&blocks, "machine learning", &en());
        assert!(sem_kept.len() >= bm25_kept.len());
    }

    #[test]
    fn filter_semantic_preserves_doc_order() {
        let blocks = vec![
            para("Alpha block about machine learning"),
            para("Beta block about weather"),
            para("Gamma block about neural networks"),
        ];
        let (kept, _) = filter_semantic(&blocks, "machine learning", &en());
        // Kept blocks should be in document order (by index).
        for w in kept.windows(2) {
            assert!(
                blocks.iter().position(|b| std::ptr::eq(b, w[0]))
                    <= blocks.iter().position(|b| std::ptr::eq(b, w[1]))
            );
        }
    }

    // ── Section Gravity tests ──

    fn heading(level: u8, text: &str, path: Vec<&str>) -> Block {
        Block::Heading {
            level,
            text: text.to_string(),
            path: path.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn para_in(text: &str, path: Vec<&str>) -> Block {
        Block::Para {
            md: text.to_string(),
            link_density: 0.0,
            path: path.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn section_gravity_heading_match_keeps_entire_section() {
        // Heading matches query: entire section (heading + all body)
        // is kept, even body blocks with zero BM25 score.
        let blocks = vec![
            heading(1, "Installation", vec!["Installation"]),
            para_in("Run the installer.", vec!["Installation"]),
            para_in("Follow the prompts on screen.", vec!["Installation"]),
            heading(1, "Usage", vec!["Usage"]),
            para_in("Type commands to use the tool.", vec!["Usage"]),
        ];
        let (kept, fell_back) = filter_semantic(&blocks, "installation", &en());
        assert!(!fell_back);
        // Section "Installation" has 3 blocks (heading + 2 paras).
        // Section "Usage" has 0 matches (heading and body don't match).
        assert_eq!(kept.len(), 3);
        assert!(kept.iter().any(|b| b.text().contains("Installation")));
        assert!(kept.iter().any(|b| b.text().contains("installer")));
        assert!(kept.iter().any(|b| b.text().contains("prompts")));
        // "Usage" section should be dropped.
        assert!(!kept.iter().any(|b| b.text().contains("commands")));
    }

    #[test]
    fn inverse_gravity_body_match_keeps_heading() {
        // Body block matches but heading does not: keep the body
        // block AND its heading (for context), drop the rest of
        // the section.
        let blocks = vec![
            heading(2, "History", vec!["Guide", "History"]),
            para_in("The project started in 2010.", vec!["Guide", "History"]),
            para_in("Ownership was introduced later.", vec!["Guide", "History"]),
            para_in("The logo was redesigned.", vec!["Guide", "History"]),
        ];
        // Query "ownership" matches only block 2 (body), not the heading.
        let (kept, fell_back) = filter_semantic(&blocks, "ownership", &en());
        assert!(!fell_back);
        // Should keep heading "History" + matching body block.
        // Non-matching body blocks should be dropped.
        assert!(kept.iter().any(|b| b.text().contains("Ownership")));
        assert!(kept.iter().any(|b| b.text().contains("History")));
        // Non-matching body blocks should be dropped.
        assert!(!kept.iter().any(|b| b.text().contains("started")));
        assert!(!kept.iter().any(|b| b.text().contains("logo")));
    }

    #[test]
    fn breadcrumb_expansion_adds_parent_headings() {
        // A body block deep in a heading hierarchy is kept. Its
        // parent heading (which is in a different section) must
        // also be kept for structural context.
        let blocks = vec![
            heading(1, "Features", vec!["Features"]),
            para_in("Overview of features.", vec!["Features"]),
            heading(2, "Memory", vec!["Features", "Memory"]),
            para_in(
                "The borrow checker prevents errors.",
                vec!["Features", "Memory"],
            ),
        ];
        // Query "borrow checker" matches block 3 (body under "Memory").
        let (kept, fell_back) = filter_semantic(&blocks, "borrow checker", &en());
        assert!(!fell_back);
        // Block 3 (matching body) must be kept.
        assert!(kept.iter().any(|b| b.text().contains("borrow checker")));
        // Heading "Memory" (section heading) must be kept (Inverse Gravity).
        assert!(kept.iter().any(|b| b.text().contains("Memory")));
        // Heading "Features" (parent heading) must be kept (Breadcrumb).
        assert!(kept.iter().any(|b| b.text().contains("Features")));
    }

    #[test]
    fn non_matching_sections_dropped() {
        // Sections with no keyword match in heading or body are
        // dropped entirely.
        let blocks = vec![
            heading(1, "Installation", vec!["Installation"]),
            para_in("Run setup.exe.", vec!["Installation"]),
            heading(1, "Cooking Recipes", vec!["Cooking Recipes"]),
            para_in("How to make pasta.", vec!["Cooking Recipes"]),
            heading(1, "Troubleshooting", vec!["Troubleshooting"]),
            para_in(
                "If installation fails, check logs.",
                vec!["Troubleshooting"],
            ),
        ];
        let (kept, fell_back) = filter_semantic(&blocks, "installation", &en());
        assert!(!fell_back);
        assert!(kept.iter().any(|b| b.text().contains("Installation")));
        assert!(kept.iter().any(|b| b.text().contains("setup")));
        // "Cooking Recipes" section: no match, fully dropped.
        assert!(!kept.iter().any(|b| b.text().contains("pasta")));
        // "Troubleshooting" section: body mentions "installation",
        // so it should be kept (body match + heading).
        assert!(kept.iter().any(|b| b.text().contains("Troubleshooting")));
        assert!(kept.iter().any(|b| b.text().contains("logs")));
    }

    #[test]
    fn section_gravity_preamble_blocks() {
        // Blocks before the first heading (preamble) are scored as
        // a section with no heading. Matching blocks are kept.
        let blocks = vec![
            para("Introduction to the tool."),
            para("It uses ownership for memory safety."),
            heading(1, "Setup", vec!["Setup"]),
            para_in("Install via cargo.", vec!["Setup"]),
        ];
        let (kept, fell_back) = filter_semantic(&blocks, "ownership", &en());
        assert!(!fell_back);
        // Preamble block matching "ownership" is kept.
        assert!(kept.iter().any(|b| b.text().contains("ownership")));
        // Non-matching preamble block is dropped.
        assert!(!kept.iter().any(|b| b.text().contains("Introduction")));
        // Non-matching section "Setup" is dropped.
        assert!(!kept.iter().any(|b| b.text().contains("cargo")));
    }

    // ── Code block fission tests ──

    #[test]
    fn fission_splits_large_json_code_block() {
        // A large JSON code block should be split into sub-blocks
        // at top-level key boundaries.
        let json = "".to_string()
            + "{\n"
            + "  \"$schema\": \"http://example.com\",\n"
            + "  \"name\": \"test\",\n"
            + "  \"description\": \""
            + &"x".repeat(1800)
            + "\",\n"
            + "  \"mcp\": {\n"
            + "    \"command\": \"node\",\n"
            + "    \"args\": [\"server.js\"]\n"
            + "  },\n"
            + "  \"$defs\": {\n"
            + "    \"ServerConfig\": {\n"
            + "      \"type\": \"object\",\n"
            + "      \"properties\": {\n"
            + "        \"command\": { \"type\": \"string\" }\n"
            + "      }\n"
            + "    }\n"
            + "  }\n"
            + "}";
        assert!(json.len() > 2000);
        let block = Block::Code {
            lang: Some("json".to_string()),
            code: json,
            path: vec![],
        };
        let expanded = expand_code_blocks(vec![block]);
        assert!(
            expanded.len() > 1,
            "large JSON should be split into sub-blocks, got {}",
            expanded.len()
        );
    }

    #[test]
    fn fission_no_split_small_code() {
        // Small code blocks are passed through unchanged.
        let code = "fn main() { println!(\"hello\"); }";
        let block = Block::Code {
            lang: Some("rust".to_string()),
            code: code.to_string(),
            path: vec![],
        };
        let expanded = expand_code_blocks(vec![block]);
        assert_eq!(expanded.len(), 1);
    }

    #[test]
    fn fission_no_code_blocks_passthrough() {
        // Blocks without code blocks are passed through unchanged.
        let blocks = vec![
            heading(1, "Title", vec!["Title"]),
            para_in("Some content.", vec!["Title"]),
        ];
        let expanded = expand_code_blocks(blocks.clone());
        assert_eq!(expanded.len(), blocks.len());
    }

    #[test]
    fn offload_inference_value_inline_outside_runtime() {
        // Outside any Tokio runtime the work runs inline and passes through.
        assert_eq!(offload_inference(|| 41u8), 41);
    }

    #[test]
    fn offload_inference_ok_inside_multithread_worker() {
        // block_in_place is the legal path here: it must run the work and
        // never panic on a multi-thread worker.
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async move {
            assert_eq!(offload_inference(|| 41u8), 41);
        });
    }

    #[test]
    fn offload_inference_inline_on_current_thread() {
        // block_in_place panics on current-thread runtimes; the flavor guard
        // must keep us inline there instead.
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let tid = std::thread::current().id();
            let inner_tid = offload_inference(|| std::thread::current().id());
            assert_eq!(inner_tid, tid, "current_thread must stay inline");
        });
    }

    #[test]
    fn offload_timer_survives_inference_on_single_worker() {
        // The discriminating test: on a one-worker runtime, a sync "inference"
        // burst must not starve a small timer. Without the offload the timer
        // waits for the whole burst; with it the runtime supplies a
        // replacement worker and the timer fires on time.
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let started = std::time::Instant::now();
            let timer = tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_millis(30)).await;
                started.elapsed()
            });
            let burst = tokio::spawn(async move {
                offload_inference(|| {
                    std::thread::sleep(std::time::Duration::from_millis(150));
                    7u8
                })
            });
            assert_eq!(burst.await.unwrap(), 7);
            let timer_elapsed = timer.await.unwrap();
            assert!(
                timer_elapsed < std::time::Duration::from_millis(140),
                "timer starved: {:?}",
                timer_elapsed
            );
        });
    }
}
