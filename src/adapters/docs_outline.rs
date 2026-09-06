//! Docs-framework adapter: mkdocs / Docusaurus / Sphinx / Antora
//! sites carry a nav sidebar that IS the site map. When detected,
//! the output gains a compact `Site outline` : the agent sees the
//! whole doc tree (and via L-handles, cheap links into it) before
//! deciding what to read next. Composes with crawl's map phase.

use scraper::{ElementRef, Html, Selector};

use crate::extract::{ContentKind, ExtractOptions, Extracted};

const MAX_ENTRIES: usize = 40;

/// Which framework this page declares, if any.
enum Framework {
    MkDocs,
    Docusaurus,
    Sphinx,
    Antora,
}

pub fn extract(html: &str, url: &str, opts: &ExtractOptions) -> Option<Extracted> {
    if opts.selector.is_some() {
        return None;
    }
    let doc = Html::parse_document(html);
    let fw = detect(&doc)?;

    // Pull the nav: framework-specific container, generic <nav>
    // fallback. Validate: ≥5 internal links or it's not a docs tree.
    let nav_links = match fw {
        Framework::MkDocs => nav_from(&doc, ".md-nav__link, nav.md-nav a"),
        Framework::Docusaurus => nav_from(&doc, ".menu__link, nav .navbar__inner a, aside a"),
        Framework::Sphinx => nav_from(&doc, ".toctree-l1 a, .sphinxsidebar a, nav a"),
        Framework::Antora => nav_from(&doc, ".nav .item a, aside.nav a"),
    };

    // Render the outline. Version-switcher entries (bare semver
    // labels) are picker UI, not pages : drop them; dedupe repeats.
    let mut outline = String::from("## Site outline\n\n");
    let mut n = 0;
    let mut seen: Vec<(String, String)> = Vec::new();
    for (depth, text, href) in nav_links.into_iter() {
        if n >= MAX_ENTRIES {
            break;
        }
        let bare_version = {
            let t = text.trim_start_matches('v');
            !t.is_empty()
                && t.chars().next().is_some_and(|c| c.is_ascii_digit())
                && t.chars().all(|c| c.is_ascii_digit() || c == '.')
        };
        if bare_version && !href.ends_with(&format!("/{text}")) {
            continue;
        }
        let key = (text.clone(), href.clone());
        if seen.contains(&key) {
            continue;
        }
        seen.push(key);
        let indent = "  ".repeat(depth.min(3));
        outline.push_str(&format!("{indent}- [{text}]({href})\n"));
        n += 1;
    }
    if n < 5 {
        return None; // not a real docs nav
    }
    outline.push('\n');
    let _ = fw;

    // Generic extraction of the MAIN content, with the outline
    // prepended. We don't re-implement DonSift here : instead the
    // adapter mutates the HTML: strip the nav/sidebar/footer so
    // the generic pipeline focuses on content, and prepend the
    // outline as a leading heading block.
    let content_sel = Selector::parse(
        "main, article, .md-content, .theme-doc-markdown, div.document, .doc, body",
    )
    .unwrap();
    let root = doc.select(&content_sel).next()?;
    let mut body_opts = opts.clone();
    body_opts.max_chars = None;
    let mut body = String::new();
    let block_sel = Selector::parse("h1, h2, h3, h4, p, ul, ol, pre, table, blockquote").unwrap();
    for el in root.select(&block_sel) {
        // Descendant select: a <p> inside a <li> or <blockquote>,
        // a nested <ul>, would be emitted as part of its parent
        // AND again on its own. Outermost matches only.
        let nested = el
            .ancestors()
            .take_while(|a| a.id() != root.id())
            .filter_map(ElementRef::wrap)
            .any(|a| block_sel.matches(&a));
        if nested {
            continue;
        }
        match el.value().name() {
            "h1" | "h2" | "h3" | "h4" => {
                let t = text_of(el);
                if !t.is_empty() {
                    let level = el.value().name().as_bytes()[1] - b'0';
                    body.push_str(&format!("{} {}\n\n", "#".repeat(level as usize), t));
                }
            }
            "ul" | "ol" => {
                let (items, _) = crate::extract::blocks::list_items(el, url, &body_opts, 0);
                if !items.is_empty() {
                    crate::extract::render::push_list(&mut body, &items, el.value().name() == "ol");
                }
            }
            _ => {
                let (m, _) = crate::extract::inline::markdown(el, url, &body_opts);
                if !m.trim().is_empty() {
                    body.push_str(m.trim());
                    body.push_str("\n\n");
                }
            }
        }
    }
    if body.trim().is_empty() {
        return None;
    }

    let full = format!("{outline}{body}");
    let total = full.len();
    let max = opts.max_chars.unwrap_or(16_000).max(200);
    let (slice, next) = crate::extract::paginate_public(&full, opts.offset, max);
    Some(Extracted {
        markdown: slice,
        title: doc
            .select(&Selector::parse("title").unwrap())
            .next()
            .map(text_of),
        byline: None,
        published: None,
        site: None,
        total_chars: total,
        next_offset: next,
        blocks_total: n,
        blocks_shown: n,
        tokens_est: total / 4,
        thin: false,
        content_kind: ContentKind::Docs,
        lang: "en".to_string(),
        quality: 0.85,
        pdf_pages: None,
        images: Vec::new(),
        fingerprint: None,
        via: Some("adapter:docs-nav"),
    })
}

/// Framework detection from generator meta / body classes.
fn detect(doc: &Html) -> Option<Framework> {
    let gen_sel = Selector::parse("meta[name='generator']").ok()?;
    if let Some(g) = doc.select(&gen_sel).next()
        && let Some(content) = g.value().attr("content")
    {
        let c = content.to_lowercase();
        if c.contains("mkdocs") {
            return Some(Framework::MkDocs);
        }
        if c.contains("sphinx") {
            return Some(Framework::Sphinx);
        }
        if c.contains("antora") {
            return Some(Framework::Antora);
        }
        if c.contains("docusaurus") {
            return Some(Framework::Docusaurus);
        }
    }
    // Docusaurus doesn't always declare a generator: detect by
    // __docusaurus script + .menu__link.
    let dq = Selector::parse("script[src*='docusaurus'], a.menu__link").ok()?;
    if doc.select(&dq).next().is_some() {
        return Some(Framework::Docusaurus);
    }
    None
}

/// Nav entries as (depth, text, href) : depth from class
/// markers when present, else nesting level.
fn nav_from(doc: &Html, selectors: &str) -> Vec<(usize, String, String)> {
    let sel = match Selector::parse(selectors) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    let base = url::Url::parse("https://docs.invalid/").unwrap();
    let mut out = Vec::new();
    for a in doc.select(&sel) {
        let text = text_of(a);
        let Some(href) = a.value().attr("href") else {
            continue;
        };
        if href.starts_with("http") || href.starts_with('#') || href.starts_with("mailto:") {
            continue; // external / anchor / mail
        }
        // Absolute-ize relative hrefs against a neutral base.
        let full = match base.join(href) {
            Ok(joined) => joined.path().to_string(),
            Err(_) => continue,
        };
        // Depth: mkdocs .md-nav__item--level-N classes, docusaurus
        // menu__link--sublist, sphinx toctree-lN, else DOM depth.
        let classes = a.value().classes().collect::<Vec<_>>();
        let mut depth = 0;
        for c in &classes {
            if let Some(rest) = c
                .strip_prefix("md-nav__item--level-")
                .or_else(|| c.strip_prefix("toctree-l"))
            {
                depth = rest.parse::<usize>().unwrap_or(1).saturating_sub(1);
            } else if *c == "menu__link--sublist" || c.starts_with("menu__list-item-") {
                depth = 1;
            }
        }
        if text.is_empty() {
            continue;
        }
        out.push((depth, text, full));
    }
    out
}

fn text_of(el: ElementRef) -> String {
    el.text().collect::<String>().trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts() -> ExtractOptions {
        ExtractOptions::default()
    }

    const MKDOCS: &str = r#"<html><head>
      <meta name="generator" content="mkdocs-1.6">
      </head><body>
      <nav>
        <a class="md-nav__item--level-1 md-nav__link" href="/">Home</a>
        <a class="md-nav__item--level-2 md-nav__link" href="/guide/">Guide</a>
        <a class="md-nav__item--level-2 md-nav__link" href="/api/">API</a>
        <a class="md-nav__item--level-3 md-nav__link" href="/api/auth/">Auth</a>
        <a class="md-nav__item--level-2 md-nav__link" href="/faq/">FAQ</a>
        <a class="md-nav__item--level-2 md-nav__link" href="/changelog/">Changelog</a>
      </nav>
      <main>
        <h1>Guide</h1>
        <p>Read the guide carefully. It has <a href="/api/">links</a>.</p>
        <pre>code sample</pre>
      </main>
      </body></html>"#;

    #[test]
    fn mkdocs_outline_renders() {
        let ex = extract(MKDOCS, "https://docs.example.com/guide/", &opts()).unwrap();
        assert_eq!(ex.via, Some("adapter:docs-nav"));
        assert!(ex.markdown.contains("## Site outline"));
        assert!(ex.markdown.contains("- [Guide](/guide/)"));
        assert!(ex.markdown.contains("  - [Auth](/api/auth/)"));
        // Content still there.
        assert!(ex.markdown.contains("Read the guide carefully"));
        assert_eq!(ex.content_kind, ContentKind::Docs);
    }

    #[test]
    fn non_docs_rejected() {
        assert!(
            extract(
                MKDOCS.replace("mkdocs-1.6", "wordpress").as_str(),
                "https://x.com/",
                &opts()
            )
            .is_none()
        );
        // Too few nav entries.
        let thin = r#"<html><head><meta name="generator" content="mkdocs-1.6"></head>
          <body><nav><a class="md-nav__link" href="/a/">A</a><a class="md-nav__link" href="/b/">B</a></nav>
          <main><p>hi</p></main></body></html>"#;
        assert!(extract(thin, "https://docs.example.com/", &opts()).is_none());
    }

    // The block selector matched descendants, so a <p> inside a
    // <li> or <blockquote>, and a nested <ul>, were emitted once
    // as part of their parent and again on their own.
    #[test]
    fn nested_blocks_are_emitted_once() {
        let html = MKDOCS.replace(
            "<pre>code sample</pre>",
            "<ul><li><p>Step one</p><ul><li>Detail a</li></ul></li><li>Step two</li></ul>\
             <blockquote><p>Quoted note</p></blockquote>\
             <pre>code sample</pre>",
        );
        let ex = extract(&html, "https://docs.example.com/guide/", &opts()).unwrap();
        let md = &ex.markdown;
        assert_eq!(md.matches("Step one").count(), 1, "{md}");
        assert_eq!(md.matches("Detail a").count(), 1, "{md}");
        assert_eq!(md.matches("Quoted note").count(), 1, "{md}");
        assert!(
            md.contains("- Step one\n  - Detail a\n- Step two\n"),
            "{md}"
        );
    }
}
