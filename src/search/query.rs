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
    fn filetype_filter_keeps_matching_and_extensionless() {
        let mut rs = vec![
            m("https://a.example/paper.pdf", "Paper"),
            m("https://b.example/page.html", "HTML"),
            m("https://c.example/download?id=1", "Download"),
        ];
        filetype_filter("paper filetype:pdf", &mut rs);
        assert_eq!(rs.len(), 2, "pdf + extensionless kept: {rs:?}");
    }
}
