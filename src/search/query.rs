//! Query compiler (v4 C2): one AST → per-engine params.
//!
//! Operators (`site:`, `filetype:`, `intitle:`) only ride engines
//! that honor them. Engines that ignore an operator get the free-text
//! form so BM25 never ranks the literal token `site:github.com` as
//! a term. Post-merge `site_filter` stays as the fail-closed belt.

/// Parsed operator set + free text. `text` never contains the
/// stripped operators.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompiledQuery {
    /// Original query, untouched (cache keys, site_filter, intent).
    pub raw: String,
    /// Free text with operators removed, whitespace collapsed.
    pub text: String,
    pub site: Option<String>,
    pub filetype: Option<String>,
    pub intitle: Option<String>,
}

impl CompiledQuery {
    pub fn has_operators(&self) -> bool {
        self.site.is_some() || self.filetype.is_some() || self.intitle.is_some()
    }
}

/// Per-engine operator honesty. Default is "honor all": sending an
/// operator to an engine that ignores it costs a worse query, while
/// stripping one from an engine that honors it loses the filter.
/// DDG lite is the known liar: it treats `site:` as a search term.
pub fn engine_honors(engine: &str) -> (bool, bool, bool) {
    match engine {
        "ddg" | "ddg_lite" => (false, false, false),
        "ddg_html" => (true, false, false),
        _ => (true, true, true),
    }
}

/// Split operators out of the raw query.
///
/// First occurrence wins for each operator family. Values are
/// trimmed of trailing `/` (common `site:example.com/` typo) and
/// empty values are ignored.
pub fn compile(raw: &str) -> CompiledQuery {
    let mut site = None;
    let mut filetype = None;
    let mut intitle = None;
    let mut words: Vec<&str> = Vec::new();
    // Peekable so a quoted operator value spanning several
    // whitespace tokens (`intitle:"HTTP status codes"`) can be
    // reassembled: split_whitespace alone kept only the first word
    // ("HTTP") as the title and leaked `status codes"` (stray quote
    // and all) into free-text ranking, and `for_engine`'s multi-word
    // `intitle:"…"` branch was dead because compile never produced
    // one.
    let mut it = raw.split_whitespace().peekable();
    while let Some(token) = it.next() {
        let lower = token.to_ascii_lowercase();
        if let Some(rest) = lower.strip_prefix("site:") {
            let v = rest.trim_end_matches('/');
            if !v.is_empty() && site.is_none() {
                site = Some(v.to_string());
            }
            // Always consume the operator token (empty, duplicate, or
            // taken): it must never pollute free-text ranking.
            continue;
        }
        if let Some(rest) = lower.strip_prefix("filetype:") {
            let v = rest.trim_start_matches('.').trim_end_matches('/');
            if !v.is_empty() && filetype.is_none() {
                filetype = Some(v.to_string());
            }
            continue;
        }
        if lower.starts_with("intitle:") {
            // Original-case value after the first colon (the operator
            // name is ASCII, so this split is byte-safe).
            let value = token.split_once(':').map(|(_, v)| v).unwrap_or("");
            let phrase = if let Some(open) = value.strip_prefix('"') {
                // Quoted value: closes in this token, or spans the
                // following whitespace tokens until the closing quote
                // (unterminated = best-effort, take what is there).
                if let Some(inner) = open.strip_suffix('"') {
                    inner.to_string()
                } else {
                    let mut parts = vec![open.to_string()];
                    for next in it.by_ref() {
                        if let Some(head) = next.strip_suffix('"') {
                            parts.push(head.to_string());
                            break;
                        }
                        parts.push(next.to_string());
                    }
                    parts.join(" ")
                }
            } else {
                value.trim_end_matches('/').to_string()
            };
            let phrase = phrase.trim();
            if !phrase.is_empty() && intitle.is_none() {
                intitle = Some(phrase.to_string());
            }
            continue;
        }
        words.push(token);
    }
    let text = words.join(" ");
    CompiledQuery {
        raw: raw.to_string(),
        text,
        site,
        filetype,
        intitle,
    }
}

/// Build the `q=` value for one engine. Honored operators stay in
/// the query string (engine-side filter); the rest are stripped so
/// free-text ranking is not polluted.
pub fn for_engine(c: &CompiledQuery, engine: &str) -> String {
    if !c.has_operators() {
        return c.raw.clone();
    }
    if !crate::config::cfg().search.query_compile {
        return c.raw.clone();
    }
    let (honors_site, honors_file, honors_title) = engine_honors(engine);
    let mut parts: Vec<String> = Vec::new();
    if let Some(s) = &c.site
        && honors_site
    {
        parts.push(format!("site:{s}"));
    }
    if let Some(f) = &c.filetype
        && honors_file
    {
        parts.push(format!("filetype:{f}"));
    }
    if let Some(t) = &c.intitle
        && honors_title
    {
        // Quote multi-word titles so engines parse them as one term.
        if t.contains(' ') {
            parts.push(format!("intitle:\"{t}\""));
        } else {
            parts.push(format!("intitle:{t}"));
        }
    }
    // Free text first: engines that stop at the first operator still
    // see the user's actual words.
    if !c.text.is_empty() {
        parts.insert(0, c.text.clone());
    }
    if parts.is_empty() {
        // Every operator was stripped and there is no free text
        // (`site:example.com` alone on DDG lite). Degrade to the
        // site value as a bare term: searching "example.com" is
        // useful; sending the literal `site:` token is not.
        if let Some(s) = &c.site {
            return s.clone();
        }
        return c.text.clone();
    }
    parts.join(" ")
}

/// Belt for `intitle:`: drop hits whose title does not contain the
/// phrase. Engines that honor it still leak; fail closed like site:.
pub(crate) fn intitle_filter(query: &str, results: &mut Vec<super::rank::Merged>) {
    let c = compile(query);
    let Some(needle) = c.intitle else {
        return;
    };
    let needle_l = needle.to_lowercase();
    results.retain(|r| r.title.to_lowercase().contains(&needle_l));
}

/// `filetype:` soft filter: keep URLs that look like the type or
/// that have no extension (redirect wrappers, download handlers).
/// Hard-dropping extensionless URLs would empty most SERPs.
pub(crate) fn filetype_filter(query: &str, results: &mut Vec<super::rank::Merged>) {
    let c = compile(query);
    let Some(ft) = c.filetype else {
        return;
    };
    let ft = ft.to_lowercase();
    results.retain(|r| {
        let path = url::Url::parse(&r.url)
            .map(|u| u.path().to_ascii_lowercase())
            .unwrap_or_else(|_| r.url.to_ascii_lowercase());
        // Explicit other extension → drop. No extension or matching
        // extension → keep.
        match path.rsplit_once('.') {
            Some((_, ext)) if !ext.contains('/') && ext.len() <= 5 => ext == ft,
            _ => true,
        }
    });
}

/// Split a `site:` value into the host to match and an optional path
/// prefix (with its leading `/`). A scheme is ignored:
/// `https://docs.rs` scopes to the host `docs.rs`, and
/// `github.com/tokio-rs` scopes to github.com paths starting with
/// `/tokio-rs`. A value with no host before its first `/` is
/// returned whole as the host, so it matches nothing.
pub(crate) fn site_scope(site: &str) -> (&str, Option<&str>) {
    let rest = match site.split_once("://") {
        Some((scheme, rest)) if !scheme.contains('/') => rest,
        _ => site,
    };
    match rest.find('/') {
        Some(at) if at > 0 => (&rest[..at], Some(&rest[at..])),
        _ => (rest, None),
    }
}

/// True when `url`'s path starts with the `site:` path prefix, or
/// when there is none. `prefix` comes lowercased from the query, so
/// the path is compared ASCII case-insensitively.
pub(crate) fn site_path_matches(url: &url::Url, prefix: Option<&str>) -> bool {
    prefix.is_none_or(|prefix| url.path().to_ascii_lowercase().starts_with(prefix))
}

/// site: routes to a matching vertical so a scoped search gets the
/// site's own API instead of only web-engine leftovers.
pub(crate) fn site_vertical(site: &str) -> Option<&'static str> {
    let s = site.trim_start_matches("www.").to_ascii_lowercase();
    if s == "github.com" || s.ends_with(".github.com") {
        return Some("github");
    }
    if s == "stackoverflow.com"
        || s == "superuser.com"
        || s == "serverfault.com"
        || s == "askubuntu.com"
        || s == "stackexchange.com"
        || s.ends_with(".stackexchange.com")
    {
        return Some("stackexchange");
    }
    if s == "wikipedia.org" || s.ends_with(".wikipedia.org") {
        return Some("wikipedia");
    }
    if s == "news.ycombinator.com" {
        return Some("hn");
    }
    if s == "arxiv.org" {
        return Some("arxiv");
    }
    if s == "developer.mozilla.org" || s == "mdn.io" {
        return Some("mdn");
    }
    None
}

/// The query as the sources should receive it.
///
/// A pasted URL is a question about that page and its site: it becomes
/// `site:<host>` plus the path's words, because engines read a raw URL as
/// noise (`https://docs.rs/tokio` ranked Kubernetes pages first). Emoji carry
/// no searchable meaning and are dropped when words remain ("🦀 rust" left two
/// results); a query that is only symbols is left as typed.
pub fn normalize(query: &str) -> String {
    let t = query.trim();
    if !t.contains(char::is_whitespace)
        && let Ok(u) = url::Url::parse(t)
        && matches!(u.scheme(), "http" | "https")
        && let Some(host) = u.host_str()
    {
        let mut q = format!("site:{}", host.trim_start_matches("www."));
        let mut seen: Vec<String> = Vec::new();
        for word in u.path().split(|c: char| !c.is_alphanumeric()) {
            let w = word.to_lowercase();
            if w.chars().count() < 2
                || matches!(
                    w.as_str(),
                    "html" | "htm" | "php" | "aspx" | "index" | "www"
                )
                || seen.contains(&w)
            {
                continue;
            }
            q.push(' ');
            q.push_str(&w);
            seen.push(w);
        }
        return q;
    }
    let is_pictograph = |c: char| {
        matches!(c as u32,
            0x1F000..=0x1FAFF | 0x2600..=0x27BF | 0x2B00..=0x2BFF | 0xFE0F | 0x200D | 0x20E3)
    };
    let stripped = t
        .chars()
        .map(|c| if is_pictograph(c) { ' ' } else { c })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if stripped.chars().any(char::is_alphanumeric) {
        stripped
    } else {
        t.to_string()
    }
}

/// The scripts this matcher distinguishes. Only the CJK family changes the
/// matching rule; the rest are recorded so a locale or language decision can
/// read them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Script {
    Latin,
    Kana,
    Han,
    Hangul,
    Cyrillic,
    Arabic,
    Devanagari,
    Other,
}

fn is_kana(c: char) -> bool {
    ('\u{3040}'..='\u{30ff}').contains(&c)
}

fn is_han(c: char) -> bool {
    ('\u{4e00}'..='\u{9fff}').contains(&c)
        || ('\u{3400}'..='\u{4dbf}').contains(&c)
        || ('\u{f900}'..='\u{faff}').contains(&c)
}

fn is_hangul(c: char) -> bool {
    ('\u{ac00}'..='\u{d7af}').contains(&c)
}

pub(crate) fn is_cjk(c: char) -> bool {
    is_kana(c) || is_han(c) || is_hangul(c)
}

/// The script a text is written in, by its first character that has one. A query
/// is judged by what it is written in, and a hit by whether it shares that.
pub fn script_of(text: &str) -> Script {
    for c in text.chars() {
        if is_kana(c) {
            return Script::Kana;
        }
        if is_hangul(c) {
            return Script::Hangul;
        }
        if is_han(c) {
            return Script::Han;
        }
        if ('\u{0400}'..='\u{04ff}').contains(&c) {
            return Script::Cyrillic;
        }
        if ('\u{0600}'..='\u{06ff}').contains(&c) {
            return Script::Arabic;
        }
        if ('\u{0900}'..='\u{097f}').contains(&c) {
            return Script::Devanagari;
        }
        if ('\u{0370}'..='\u{03ff}').contains(&c) {
            return Script::Other;
        }
    }
    Script::Latin
}

/// Words that carry no topic: function words and the temporal vocabulary of a
/// news query. A title containing "this week" is not about the query because
/// the query also said "week".
const STOP: &[&str] = &[
    "the",
    "and",
    "for",
    "how",
    "what",
    "who",
    "why",
    "when",
    "where",
    "which",
    "does",
    "did",
    "can",
    "could",
    "should",
    "would",
    "with",
    "without",
    "from",
    "into",
    "about",
    "best",
    "vs",
    "versus",
    "you",
    "your",
    "are",
    "was",
    "were",
    "is",
    "it",
    "its",
    "this",
    "that",
    "these",
    "those",
    "there",
    "of",
    "to",
    "in",
    "on",
    "or",
    "a",
    "an",
    "latest",
    "today",
    "tonight",
    "yesterday",
    "week",
    "breaking",
    "recent",
    "update",
    "updates",
    "news",
    "live",
];

/// Capitalised words that are sentence furniture, never a named entity.
const ANCHOR_STOP: &[&str] = &[
    "the", "a", "an", "how", "what", "who", "why", "when", "where", "which", "is", "are", "was",
    "were", "do", "does", "did", "can", "could", "should", "would", "this", "that", "these",
    "those", "it", "its", "of", "to", "in", "on", "for", "and", "or", "with", "without", "from",
    "into", "about",
];

/// What kind of question the query asks. Fixed, small and exhaustive: every
/// query gets exactly one, and it decides the program (which steps run), the
/// lanes allowed to contribute, and the kinds of result the ranker expects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shape {
    /// One name — `select!`, `itertools::izip`, `@types/node`. Answered by the
    /// record and the documentation, never by a page about the word.
    Identifier,
    /// Code help: an error, an API, a language, a build.
    Code,
    /// A dated event: the wire, the feeds, the publisher's own page.
    News,
    /// A paper or a study.
    Paper,
    /// A procedure: "how to…".
    HowTo,
    /// A price, a model number, a buying decision.
    Shopping,
    /// A named thing: what/who/when.
    Entity,
    /// Everything else.
    Web,
}

impl Shape {
    /// The word the census, `--explain` and the bench use for it.
    pub fn label(self) -> &'static str {
        match self {
            Shape::Identifier => "identifier",
            Shape::Code => "code",
            Shape::News => "news",
            Shape::Paper => "paper",
            Shape::HowTo => "howto",
            Shape::Shopping => "shopping",
            Shape::Entity => "entity",
            Shape::Web => "web",
        }
    }

    /// The kinds of result that answer it, strongest first. `kind` is the
    /// result-kind classifier's vocabulary in [`super::rank`].
    pub fn expected_kinds(self) -> &'static [&'static str] {
        match self {
            // A name is answered by its record or its docs; a paper's abstract
            // page answers a paper query; the rest is the open web.
            Shape::Identifier => &["record"],
            Shape::Paper => &["record", "publisher"],
            Shape::News => &["publisher", "record"],
            Shape::Code => &["record", "community"],
            Shape::HowTo => &["publisher", "community"],
            Shape::Shopping => &["publisher", "community"],
            Shape::Entity => &["record", "publisher"],
            // The residual shape expects no kind: listing three of five made
            // "matches the shape" mean "is not an ordinary site", and lifted a
            // tangential encyclopedia article over the weather service a
            // `東京の天気予報` query asked for (a bench gold row).
            Shape::Web => &[],
        }
    }
}

/// Openings that make a query a procedure, whatever the words after them are.
const HOWTO_LEADS: &[&str] = &["how to ", "how do ", "how does ", "how can ", "how should "];

/// Words that make a query a purchase rather than a topic.
const SHOPPING_WORDS: &[&str] = &[
    "price",
    "prices",
    "pricing",
    "buy",
    "cheap",
    "cheapest",
    "deal",
    "deals",
    "coupon",
    "review",
    "reviews",
    "specs",
    "specifications",
    "refurbished",
    "cost",
];

/// One weighted query term. CJK terms are character bigrams; a Latin term is a
/// word. The weight is 2 for a quoted phrase, 1 otherwise.
#[derive(Debug, Clone)]
struct Term {
    text: String,
    weight: f64,
}

/// The query, compiled once into the facts every later judgement reads.
///
/// This replaces the contract's `Profile`, which was the same machine wearing a
/// filter's name: it judged hits one at a time and threw away what it did not
/// recognise. The facts here are *about the query* — its terms and their
/// weights, its named anchors, its negations, its operators, its script — and
/// the judgement lives where it belongs: a veto list per hit and a set-level
/// class in [`super::quality`].
#[derive(Debug, Clone)]
pub struct QueryFacts {
    raw: String,
    compiled: CompiledQuery,
    terms: Vec<Term>,
    /// Named entities: capitalised runs (an internal capital counts even in the
    /// first word, so "SpaceX" is one and a sentence-start "Latest" is not) and
    /// quoted spans.
    anchors: Vec<String>,
    /// `-word` exclusions.
    negated: Vec<String>,
    script: Script,
}

impl QueryFacts {
    pub fn new(query: &str) -> Self {
        let compiled = compile(query);
        let mut terms: Vec<Term> = Vec::new();
        let mut anchors: Vec<String> = Vec::new();
        let mut negated: Vec<String> = Vec::new();

        // An operator's own name is never a term. `site:` names a host (and,
        // with a path, some content), `filetype:` names a format, `intitle:`
        // names a title; all three are read once, from `compile` — the same
        // parse the post-merge filters enforce. Reading the raw tokens instead
        // ranked the literal word "intitle": measured, `intitle:"Attention Is
        // All You Need" arxiv` returned nothing while three lanes answered,
        // because no result could contain the operator's own name.
        if let Some(site) = &compiled.site
            && let Some((_host, path)) = site.split_once('/')
        {
            for word in path.split(|c: char| !c.is_alphanumeric()) {
                let word = word.to_lowercase();
                if word.chars().count() >= 2 && !STOP.contains(&word.as_str()) {
                    push_term(&mut terms, word, 1.0);
                }
            }
        }
        if let Some(title) = &compiled.intitle {
            anchors.push(title.to_lowercase());
        }

        let tokens: Vec<&str> = compiled.text.split_whitespace().collect();
        let mut first_word = true;
        let mut i = 0;
        while i < tokens.len() {
            let raw = tokens[i];
            i += 1;
            if let Some(rest) = raw.strip_prefix('-')
                && !rest.is_empty()
            {
                negated.push(rest.trim_matches('"').to_lowercase());
                continue;
            }
            // A quoted span is one unit, however many words it spans: it names
            // an anchor and enters the terms as a phrase, so `"the quick brown
            // fox"` cannot be satisfied by a page carrying three of its words
            // in any order.
            if let Some(head) = raw.strip_prefix('"') {
                let mut parts: Vec<String> = Vec::new();
                if let Some(inner) = head.strip_suffix('"') {
                    parts.push(inner.to_string());
                } else {
                    parts.push(head.to_string());
                    while i < tokens.len() {
                        let next = tokens[i];
                        i += 1;
                        if let Some(inner) = next.strip_suffix('"') {
                            parts.push(inner.to_string());
                            break;
                        }
                        parts.push(next.to_string());
                    }
                }
                let phrase = parts.join(" ").trim().to_lowercase();
                if !phrase.is_empty() {
                    anchors.push(phrase.clone());
                    push_term(&mut terms, phrase.clone(), 2.0);
                    for word in phrase.split_whitespace() {
                        if word.chars().count() >= 2 && !STOP.contains(&word) {
                            push_term(&mut terms, word.to_string(), 1.0);
                        }
                    }
                }
                first_word = false;
                continue;
            }
            let quoted = raw.ends_with('"');
            let bare = raw.trim_matches('"');
            if quoted && !bare.is_empty() && bare.chars().any(char::is_whitespace) {
                anchors.push(bare.to_lowercase());
            }
            // A CJK run is one unit: bigrams, never whitespace words.
            if script_of(bare) != Script::Latin {
                for run in cjk_runs(bare) {
                    for term in bigrams(&run) {
                        push_term(&mut terms, term, 1.0);
                    }
                }
                first_word = false;
                continue;
            }
            let capitalised = bare.chars().next().is_some_and(char::is_uppercase);
            let internal_capital = bare.chars().skip(1).any(char::is_uppercase);
            let anchor_ok = capitalised
                && (internal_capital || !first_word)
                && !ANCHOR_STOP.contains(&bare.to_lowercase().as_str())
                && bare.chars().count() >= 2;
            if anchor_ok {
                anchors.push(bare.to_lowercase());
            }
            for word in bare.split(|c: char| !c.is_alphanumeric() && c != '+' && c != '#') {
                let lower = word.to_lowercase();
                if lower.is_empty() || STOP.contains(&lower.as_str()) {
                    continue;
                }
                if lower.chars().count() < 2 && !lower.chars().any(|c| c.is_numeric()) {
                    continue;
                }
                push_term(&mut terms, lower, if quoted { 2.0 } else { 1.0 });
            }
            first_word = false;
        }
        anchors.sort();
        anchors.dedup();
        negated.sort();
        negated.dedup();
        Self {
            raw: query.to_string(),
            compiled,
            terms,
            anchors,
            negated,
            script: script_of(query),
        }
    }

    pub fn raw(&self) -> &str {
        &self.raw
    }

    /// The operators the query states, and its free text without them.
    pub fn compiled(&self) -> &CompiledQuery {
        &self.compiled
    }

    /// The query's content terms, lowercased and stripped of operators — what
    /// the resolver reads to decide whether the query names one thing.
    pub fn terms(&self) -> impl Iterator<Item = &str> {
        self.terms.iter().map(|t| t.text.as_str())
    }

    /// The terms with their weights: a quoted phrase counts for more than a
    /// word. BM25+ and RM3 read this, so the query's own emphasis survives into
    /// the score instead of being flattened into a bag of words.
    pub fn weighted_terms(&self) -> impl Iterator<Item = (&str, f64)> {
        self.terms.iter().map(|t| (t.text.as_str(), t.weight))
    }

    pub fn anchors(&self) -> &[String] {
        &self.anchors
    }

    pub fn negated(&self) -> &[String] {
        &self.negated
    }

    /// The script the query is written in: the language it asked for, when it
    /// asks for one by writing in it.
    pub fn script(&self) -> Script {
        self.script
    }

    /// Whether the query is **one identifier and nothing else**: a macro
    /// (`select!`), a path (`std::mem::take`), an attribute (`#[tokio::main]`),
    /// a scoped package (`@types/node`), a version (`v1.2.3`) or a name with a
    /// code signal in it (`serde_json::Value`, `camelCase`).
    ///
    /// The shape decides what can answer: the name's own record or documentation,
    /// never a general encyclopedia or a video. `tokio::spawn` returning
    /// Wikipedia's Japanese article on the band TOKIO is the measured failure
    /// this exists to stop (5.3.0 A baseline, rows i2/i6/i7/i8).
    pub fn is_identifier(&self) -> bool {
        let text = self.compiled.text.trim();
        if text.is_empty() || text.contains(char::is_whitespace) {
            return false;
        }
        if text.contains("::") || text.starts_with('#') || text.starts_with('@') {
            return true;
        }
        if text.ends_with('!') || text.contains('_') {
            return true;
        }
        // `v1.2.3`, and only that: a bare number is not an identifier.
        let version = text.strip_prefix('v').unwrap_or(text);
        if version.chars().next().is_some_and(|c| c.is_ascii_digit())
            && version.split('.').count() >= 2
            && version
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '.')
        {
            return true;
        }
        // camelCase: a capital after the first character, lowercase around it.
        let chars: Vec<char> = text.chars().collect();
        chars.len() > 2
            && chars[0].is_lowercase()
            && chars.iter().skip(1).any(|c| c.is_uppercase())
            && chars.iter().any(|c| c.is_lowercase())
    }

    /// A path identifier split into the name that owns it and the item it
    /// names: `itertools::izip` → `("itertools", ["izip"])`, `#[tokio::main]` →
    /// `("tokio", ["main"])`, `serde_json::Value` → `("serde_json", ["Value"])`,
    /// `std::mem::take` → `("std", ["mem", "take"])`.
    ///
    /// `None` for a bare name: `select!` may be a macro of any crate, so the
    /// registry is not the thing to ask, and a scoped package is one name
    /// (`@types/node`), not an owner and an item.
    pub fn identifier_parts(&self) -> Option<(String, Vec<String>)> {
        if !self.is_identifier() {
            return None;
        }
        let text = self.compiled.text.trim();
        if text.starts_with('@') {
            return None;
        }
        let cleaned = text
            .trim_start_matches('#')
            .trim_start_matches('[')
            .trim_end_matches(']')
            .trim_end_matches('!')
            .trim();
        let segments: Vec<&str> = if cleaned.contains("::") {
            cleaned.split("::").filter(|s| !s.is_empty()).collect()
        } else if cleaned.contains('.') && !cleaned.starts_with('v') {
            // `serde_json::Value` is a path; `v1.2.3` is a version, and its
            // dots are separators of a number, not of a namespace.
            cleaned.split('.').filter(|s| !s.is_empty()).collect()
        } else {
            return None;
        };
        if segments.len() < 2 {
            return None;
        }
        let root = segments[0].trim().to_string();
        let rest: Vec<String> = segments[1..]
            .iter()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        if root.is_empty() || rest.is_empty() {
            return None;
        }
        Some((root, rest))
    }

    /// What kind of question this is. The routing vocabulary: which program
    /// answers it, which lanes may contribute, and which kinds of result are
    /// expected. Explicit user markers beat the lexicon — "how to…" is a
    /// procedure and "…price" is a purchase even when the words inside them
    /// look technical.
    pub fn shape(&self) -> Shape {
        if self.is_identifier() {
            return Shape::Identifier;
        }
        let q = self.raw.to_lowercase();
        if HOWTO_LEADS.iter().any(|lead| q.starts_with(lead)) {
            return Shape::HowTo;
        }
        if self
            .terms
            .iter()
            .any(|t| SHOPPING_WORDS.contains(&t.text.as_str()))
        {
            return Shape::Shopping;
        }
        match super::intent::detect(&self.raw) {
            super::intent::Intent::Code => Shape::Code,
            super::intent::Intent::News => Shape::News,
            super::intent::Intent::Paper => Shape::Paper,
            super::intent::Intent::Entity => Shape::Entity,
            super::intent::Intent::Web => Shape::Web,
        }
    }

    /// The query's rarest terms: long Latin words and CJK bigrams. A set that
    /// shares none of them is not about this query, whatever its numbers say.
    pub fn rare_terms(&self) -> Vec<&str> {
        let mut out: Vec<&str> = self
            .terms
            .iter()
            .filter(|t| t.text.chars().any(is_cjk) || t.text.chars().count() >= 6)
            .map(|t| t.text.as_str())
            .collect();
        out.sort_unstable();
        out.dedup();
        out.truncate(6);
        out
    }

    /// Does `root` name one of the query's terms? `_` and `-` are the same in a
    /// package name.
    pub(crate) fn names(&self, root: &str) -> bool {
        let root = root.replace('_', "-").to_lowercase();
        self.terms.iter().any(|t| {
            t.text.replace('_', "-").to_lowercase() == root
                || t.text.eq_ignore_ascii_case(root.as_str())
        })
    }

    /// How much of the query's weight a text carries, in 0..=1.
    pub fn coverage(&self, text: &str) -> f64 {
        let total: f64 = self.terms.iter().map(|t| t.weight).sum();
        if total == 0.0 {
            return 1.0;
        }
        let matched: f64 = self
            .terms
            .iter()
            .filter(|t| matches(&t.text, text))
            .map(|t| t.weight)
            .sum();
        matched / total
    }

    /// `(any anchor present, all anchors present)`. Both are true when the query
    /// names nothing.
    pub fn anchor_state(&self, text: &str) -> (bool, bool) {
        if self.anchors.is_empty() {
            return (true, true);
        }
        let hit: Vec<bool> = self.anchors.iter().map(|a| matches(a, text)).collect();
        (hit.iter().any(|b| *b), hit.iter().all(|b| *b))
    }

    /// Whether this text is *the* record the query asked for: it covers the
    /// query at 0.6 or more and carries every anchor. This is the old contract's
    /// `Verdict::Exact`, kept as a predicate because two callers (the news wire
    /// and the record-admission tier) need exactly that strength, while the
    /// ranking score uses the gradient in `contract::relevance`.
    pub fn is_exact(&self, title: &str, snippet: &str, url: &str) -> bool {
        let text = format!("{title} {snippet} {url}");
        if self
            .negated
            .iter()
            .any(|n| matches(n, &text) || text.to_lowercase().contains(n))
        {
            return false;
        }
        if self.terms.is_empty() {
            return true;
        }
        let (_, all) = self.anchor_state(&text);
        self.coverage(&text) >= 0.6 && all
    }
}

fn push_term(terms: &mut Vec<Term>, text: String, weight: f64) {
    if let Some(existing) = terms.iter_mut().find(|t| t.text == text) {
        existing.weight = existing.weight.max(weight);
    } else {
        terms.push(Term { text, weight });
    }
}

/// Runs of CJK characters inside one token, e.g. "地震速報" or "東京の".
fn cjk_runs(token: &str) -> Vec<String> {
    let mut runs = Vec::new();
    let mut current = String::new();
    for c in token.chars() {
        if is_cjk(c) {
            current.push(c);
        } else if !current.is_empty() {
            runs.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        runs.push(current);
    }
    runs
}

/// Character bigrams of a run; a one-character run is its own term.
fn bigrams(run: &str) -> Vec<String> {
    let chars: Vec<char> = run.chars().collect();
    if chars.len() <= 1 {
        return vec![run.to_string()];
    }
    chars
        .windows(2)
        .map(|w| w.iter().collect())
        .collect::<Vec<String>>()
}

/// Whether a word is English sentence furniture: never a term on its own, and
/// never an RM3 expansion term either.
pub(crate) fn is_stop_word(word: &str) -> bool {
    STOP.contains(&word)
}

/// Does `term` occur in `text`? Latin terms are whole words, with a prefix
/// match for long ones ("dataclass" finds "dataclasses"); a quoted span (a
/// term containing whitespace) is that run of words in order; CJK terms are
/// substrings, because a Japanese title has no spaces to split on.
pub(crate) fn matches(term: &str, text: &str) -> bool {
    if term.chars().any(is_cjk) {
        return text.contains(term);
    }
    // A quoted span is one term: it matches a run of words, in order, with any
    // amount of whitespace or punctuation between them.
    if term.contains(char::is_whitespace) {
        let hay = text
            .to_lowercase()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        return hay.contains(term);
    }
    let lower = text.to_lowercase();
    let words = lower.split(|c: char| !c.is_alphanumeric() && c != '+' && c != '#');
    words
        .into_iter()
        .any(|w| w == term || (term.chars().count() >= 5 && w.starts_with(term)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search::rank::Merged;

    fn m(url: &str, title: &str) -> Merged {
        Merged {
            title: title.into(),
            url: url.into(),
            snippet: String::new(),
            sources: vec![],
            score: 0.0,
            published: None,
            families: 0,
            from_page: false,
            tier: 0,
        }
    }

    #[test]
    fn compile_strips_site_and_keeps_text() {
        let c = compile("rust ownership site:doc.rust-lang.org");
        assert_eq!(c.site.as_deref(), Some("doc.rust-lang.org"));
        assert_eq!(c.text, "rust ownership");
        assert!(c.has_operators());
    }

    #[test]
    fn compile_filetype_and_intitle() {
        let c = compile("rfc 7231 filetype:pdf intitle:hypertext");
        assert_eq!(c.filetype.as_deref(), Some("pdf"));
        assert_eq!(c.intitle.as_deref(), Some("hypertext"));
        assert_eq!(c.text, "rfc 7231");
    }

    // A multi-word quoted intitle must be captured whole, not split
    // by whitespace into a one-word title with the rest ("status
    // codes\"", stray quote and all) leaking into free text.
    #[test]
    fn compile_multiword_quoted_intitle() {
        let c = compile("rfc intitle:\"HTTP status codes\" filetype:pdf");
        assert_eq!(c.intitle.as_deref(), Some("HTTP status codes"));
        assert_eq!(c.filetype.as_deref(), Some("pdf"));
        assert_eq!(
            c.text, "rfc",
            "the quoted phrase must not leak into free text"
        );
        // The for_engine multi-word branch is now reachable.
        let bing = for_engine(&c, "bing");
        assert!(
            bing.contains("intitle:\"HTTP status codes\""),
            "multi-word intitle must be re-quoted for the engine: {bing}"
        );
        // Single-token and closed-in-one-token quoted forms still work.
        assert_eq!(
            compile("intitle:hypertext").intitle.as_deref(),
            Some("hypertext")
        );
        assert_eq!(compile("intitle:\"solo\"").intitle.as_deref(), Some("solo"));
        // Unterminated quote is best-effort (takes the rest), no panic.
        assert_eq!(
            compile("intitle:\"open ended").intitle.as_deref(),
            Some("open ended")
        );
    }

    #[test]
    fn compile_trailing_slash_and_empty_ops() {
        let c = compile("site:example.com/ filetype: intitle:\"\" hello");
        assert_eq!(c.site.as_deref(), Some("example.com"));
        assert!(c.filetype.is_none());
        assert!(c.intitle.is_none());
        assert_eq!(c.text, "hello");
    }

    #[test]
    fn for_engine_keeps_ops_for_bing_strips_for_ddg_lite() {
        let c = compile("tokio site:docs.rs");
        let bing = for_engine(&c, "bing");
        assert!(bing.contains("site:docs.rs"), "bing: {bing}");
        let ddg = for_engine(&c, "ddg");
        assert!(!ddg.contains("site:"), "ddg must strip: {ddg}");
        assert!(ddg.contains("tokio"));
    }

    #[test]
    fn operator_only_query_degrades_to_site_value_on_non_honoring_engines() {
        let c = compile("site:example.com");
        let ddg = for_engine(&c, "ddg");
        assert_eq!(ddg, "example.com", "never send the literal site: token");
        let bing = for_engine(&c, "bing");
        assert_eq!(bing, "site:example.com");
    }

    #[test]
    fn site_vertical_routes_canonical_hosts() {
        assert_eq!(site_vertical("github.com"), Some("github"));
        assert_eq!(site_vertical("www.github.com"), Some("github"));
        assert_eq!(site_vertical("stackoverflow.com"), Some("stackexchange"));
        assert_eq!(site_vertical("en.wikipedia.org"), Some("wikipedia"));
        assert_eq!(site_vertical("example.com"), None);
    }

    #[test]
    fn site_scope_splits_host_from_path_and_drops_a_scheme() {
        assert_eq!(
            site_scope("github.com/tokio-rs"),
            ("github.com", Some("/tokio-rs"))
        );
        assert_eq!(site_scope("https://docs.rs"), ("docs.rs", None));
        assert_eq!(
            site_scope("http://docs.python.org/3/library"),
            ("docs.python.org", Some("/3/library"))
        );
        // Negative: a bare domain keeps no path, and a value with no
        // host stays whole so it matches no host.
        assert_eq!(site_scope("docs.rs"), ("docs.rs", None));
        assert_eq!(site_scope("/tokio-rs"), ("/tokio-rs", None));
        // A `://` inside the path is not a scheme.
        assert_eq!(
            site_scope("example.com/go?to=https://other.example"),
            ("example.com", Some("/go?to=https://other.example"))
        );
        // The site's own API is routed on the host part.
        let compiled = compile("runtime site:github.com/tokio-rs");
        let (host, _) = site_scope(compiled.site.as_deref().unwrap());
        assert_eq!(site_vertical(host), Some("github"));
    }

    #[test]
    fn site_path_matches_only_the_prefixed_subtree() {
        let url = |u: &str| url::Url::parse(u).unwrap();
        let in_scope = |u: &str| site_path_matches(&url(u), Some("/tokio-rs"));
        assert!(in_scope("https://github.com/tokio-rs/tokio"));
        assert!(in_scope("https://github.com/Tokio-RS/axum"));
        // Negative: other paths on the host are out of scope, and no
        // prefix scopes nothing.
        assert!(!in_scope("https://github.com/rust-lang/rust"));
        assert!(!in_scope("https://github.com/"));
        assert!(site_path_matches(
            &url("https://github.com/rust-lang/rust"),
            None
        ));
    }

    #[test]
    fn intitle_filter_drops_non_matching_titles() {
        let mut rs = vec![
            m("https://a.example/1", "Rust Book chapter"),
            m("https://b.example/2", "Unrelated page"),
        ];
        intitle_filter("rust intitle:book", &mut rs);
        assert_eq!(rs.len(), 1);
        assert!(rs[0].title.contains("Book"));
    }

    #[test]
    fn a_quoted_intitle_keeps_its_item_after_the_filter() {
        let mut rs = vec![m(
            "https://arxiv.org/abs/1706.03762",
            "Attention Is All You Need - arXiv.org",
        )];
        intitle_filter("intitle:\"Attention Is All You Need\" arxiv", &mut rs);
        assert_eq!(rs.len(), 1, "kept: {rs:?}");
    }

    #[test]
    fn an_operator_is_never_a_search_term() {
        let f = QueryFacts::new("intitle:\"Attention Is All You Need\" arxiv filetype:pdf");
        let terms: Vec<&str> = f.terms().collect();
        assert!(!terms.contains(&"intitle"), "terms: {terms:?}");
        assert!(!terms.contains(&"filetype"), "terms: {terms:?}");
        assert!(!terms.contains(&"pdf"), "terms: {terms:?}");
    }

    #[test]
    fn filetype_filter_keeps_matching_and_extensionless() {
        let mut rs = vec![
            m("https://a.example/paper.pdf", "Paper"),
            m("https://b.example/page.html", "HTML"),
            m("https://c.example/download?id=1", "Download"),
        ];
        filetype_filter("paper filetype:pdf", &mut rs);
        assert_eq!(rs.len(), 2, "pdf + extensionless kept: {rs:?}");
    }

    #[test]
    fn a_pasted_url_searches_its_site_for_its_path() {
        assert_eq!(normalize("https://docs.rs/tokio"), "site:docs.rs tokio");
        assert_eq!(
            normalize(" https://www.github.com/tokio-rs/tokio/index.html "),
            "site:github.com tokio rs"
        );
        assert_eq!(normalize("https://example.com/"), "site:example.com");
        // A URL inside a sentence is part of the question, not the question.
        assert_eq!(
            normalize("what is https://docs.rs/tokio"),
            "what is https://docs.rs/tokio"
        );
    }

    #[test]
    fn emoji_are_dropped_when_words_remain() {
        assert_eq!(normalize("🦀 rust"), "rust");
        assert_eq!(normalize("best ☕️ in berlin"), "best in berlin");
        assert_eq!(normalize("🦀"), "🦀");
        assert_eq!(normalize("C++ std::optional"), "C++ std::optional");
        assert_eq!(normalize("地震速報"), "地震速報");
    }
}
