//! The skip predicate: functional pruning. Junk is skipped during
//! traversal : the tree is never mutated, never re-parsed.

use scraper::ElementRef;

/// Lazily-built `math` selector for the hidden-math exception.
fn math_sel() -> &'static scraper::Selector {
    static S: std::sync::OnceLock<scraper::Selector> = std::sync::OnceLock::new();
    S.get_or_init(|| scraper::Selector::parse("math").unwrap())
}

/// Hidden containers that wrap `<math>` are the accessibility twin
/// of a rendered formula image (MediaWiki, MathJax, KaTeX all ship
/// this shape: visible SVG/PNG + hidden MathML with the LaTeX).
/// The hidden math is the ONLY machine-readable form of the
/// formula : skipping it as "hidden content" guts every technical
/// page. Skip the wrapper visually, never the math inside it.
fn has_math_descendant(el: ElementRef<'_>) -> bool {
    el.select(math_sel()).next().is_some()
}

const SKIP_TAGS: &[&str] = &[
    "script", "style", "noscript", "template", "svg", "canvas", "iframe", "object", "embed",
    "button", "input", "select", "textarea", "option", "nav", "aside",
    "footer",
    // NOTE: "form" is NOT here. Old.reddit wraps comment bodies
    // in <form class="usertext"> for the edit feature; skipping
    // <form> nukes all comment text. The form's interactive
    // children (input, button, select, textarea) are already
    // skipped above, so only text content inside forms is
    // extracted : exactly what we want.
];

const SKIP_ROLES: &[&str] = &[
    "navigation",
    "banner",
    "contentinfo",
    "complementary",
    "search",
    "dialog",
    "alert",
];

/// Class/id fragments that mark boilerplate. Long fragments use
/// substring matching on tokens; SHORT fragments (nav, menu)
/// require exact token match : "flex-nav-upsell" must not kill
/// a whole page wrapper.
///
/// NOTE: "comment" is deliberately NOT here. Discussion threads
/// (HN, forums, blogs) are primary research content for agents;
/// treating the class name as boilerplate silently dropped whole
/// comment sections from main-content scoring. Comment-section
/// noise on article pages is handled by the score competition
/// (the article container wins) : not by nuking "comment" nodes.
const NEGATIVE_SUBSTR: &[&str] = &[
    "sidebar",
    "widget",
    "footer",
    "related",
    "promo",
    "advert",
    "share",
    "social",
    "newsletter",
    "cookie",
    "modal",
    "popup",
    "banner",
    "breadcrumb",
    "masthead",
    "outbrain",
    "taboola",
    "sponsor",
    "toolbar",
    "dropdown",
    "signup",
    "infobar",
    "subscribe",
    "login",
    "signin",
];

const NEGATIVE_EXACT: &[&str] = &["nav", "menu", "sign-up", "sign-in"];

/// Ad-slot containers: a hard skip. Reddit's in-feed ad post is a
/// custom element (`shreddit-ad-post`), and the exact class/id
/// tokens below cover the common wrappers. Exact tokens only: as
/// substrings, "ads" would match "loads" and "ad" matches
/// "shadow". Never a hard skip when the element itself carries a
/// positive content token: some themes nest an ad inside a content
/// card (#288).
const AD_TAG_PREFIX: &str = "shreddit-ad";
const AD_EXACT: &[&str] = &[
    "ads",
    "adsbygoogle",
    "advert",
    "advertisement",
    "ad-slot",
    "ad-unit",
    "ad-container",
    "ad-wrapper",
    "ad-placement",
    "sponsored-post",
    "promoted-post",
    "native-ad",
];

/// Class/id fragments that mark real content (exact token match,
/// lowercased, separators normalized).
pub const POSITIVE: &[&str] = &[
    "article",
    "content",
    "main",
    "post",
    "entry",
    "story",
    "body",
    "prose",
    "markdown",
    "articlebody",
    "postcontent",
    "article-content",
    "post-content",
    "page-content",
    "main-content",
    "entry-content",
    "mw-content-text",
    "mw-parser-output",
    "contenttext",
];

fn tokens(el: &scraper::node::Element) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(id) = el.id() {
        out.push(id.to_lowercase());
    }
    if let Some(class) = el.attr("class") {
        for c in class.split_whitespace() {
            out.push(c.to_lowercase());
        }
    }
    out
}

pub fn is_positive(el: &scraper::node::Element) -> bool {
    tokens(el).iter().any(|t| {
        POSITIVE.contains(&t.as_str())
            || t.replace(['-', '_'], "") == "content"
            || t.contains("articlebody")
    })
}

fn is_negative(el: &scraper::node::Element) -> bool {
    if is_positive(el) {
        return false;
    }
    tokens(el).iter().any(|t| {
        NEGATIVE_EXACT.contains(&t.as_str()) || NEGATIVE_SUBSTR.iter().any(|n| t.contains(n))
    })
}

/// Hard skip: semantic junk only (tags, hidden, roles).
/// Class-name heuristics are NOT here : they're too fragile
/// for hard skips (a "fixed-sidebar" class can sit on the
/// main container). Use `is_negative` as a score penalty or
/// a size-gated skip instead.
pub fn skip(el: ElementRef<'_>) -> bool {
    let e = el.value();
    let name = e.name();
    if SKIP_TAGS.contains(&name) {
        // <svg> can embed MathML-adjacent content, but <math>
        // itself is never junk : the formula is content.
        return true;
    }
    // Ad slots are not content (#288): a reddit in-feed ad post or a
    // common ad wrapper never ships as prose. Exact tokens only, and
    // an element with its own positive content token wins.
    if name.starts_with(AD_TAG_PREFIX) {
        return true;
    }
    if !is_positive(e) && tokens(e).iter().any(|t| AD_EXACT.contains(&t.as_str())) {
        return true;
    }
    if name == "math" {
        return false;
    }
    if inline_popup(el) {
        return true;
    }
    // An inactive tab is hidden until its tab is clicked: its panel is
    // the page's content all the same (a pricing page's other plans).
    // A carousel's cloned slide copies a real one and stays skipped.
    if e.attr("role") == Some("tabpanel")
        && !e
            .attr("class")
            .is_some_and(|c| c.to_ascii_lowercase().contains("clone"))
    {
        return false;
    }
    if e.attr("hidden").is_some() && !has_math_descendant(el) {
        return true;
    }
    if let Some(role) = e.attr("role")
        && SKIP_ROLES.contains(&role)
        && !has_math_descendant(el)
    {
        return true;
    }
    if e.attr("aria-hidden")
        .is_some_and(|v| v.eq_ignore_ascii_case("true"))
        && !has_math_descendant(el)
    {
        return true;
    }
    // Screen-reader-only text duplicates visible content
    // (Ars/BBC badge dupes) : same class as display:none.
    if e.attr("class").is_some_and(|c| {
        c.split_whitespace().any(|t| {
            t.eq_ignore_ascii_case("sr-only")
                || t.eq_ignore_ascii_case("visually-hidden")
                || t.eq_ignore_ascii_case("visuallyhidden")
        })
    }) && !has_math_descendant(el)
    {
        return true;
    }
    if let Some(style) = e.attr("style") {
        let s: String = style.to_lowercase();
        if (s.contains("display:none")
            || s.contains("display: none")
            || s.contains("visibility:hidden"))
            && !has_math_descendant(el)
        {
            return true;
        }
    }
    false
}

/// A hover popup inside running text: a glossary definition or tooltip
/// (`<span class="term-ref">subject<span class="term-tip">…</span></span>`)
/// that a browser shows only on hover. Read inline it splices the
/// definition into the sentence. Only a span whose class names a tooltip,
/// popover or tip and whose parent is itself inline: a "tip" callout box
/// in a document is a block and stays.
fn inline_popup(el: ElementRef<'_>) -> bool {
    let e = el.value();
    if e.name() != "span" {
        return false;
    }
    let popup = e.classes().any(|c| {
        let c = c.to_ascii_lowercase();
        c.contains("tooltip")
            || c.contains("popover")
            || c.split(['-', '_']).any(|part| part == "tip")
    });
    popup
        && el
            .parent()
            .and_then(ElementRef::wrap)
            .is_some_and(|p| crate::extract::blocks::INLINE_TAGS.contains(&p.value().name()))
}

/// Class/id heuristic: LIKELY boilerplate. Caller decides
/// what to do with it (score penalty, size-gated skip).
pub fn negative(el: ElementRef<'_>) -> bool {
    is_negative(el.value())
}

/// Hard structural negative: the element's ID is a layout-region
/// name (footer, sidebar, nav...). Unlike classes : where
/// "fixed-sidebar" may style the main wrapper : an id IS the
/// region. Such containers are never the main article, at any
/// size (xkcd's 1000-char id="bottom" link farm outranked the
/// comic itself).
pub fn structural_negative(el: &scraper::node::Element) -> bool {
    let Some(id) = el.id() else {
        return false;
    };
    matches!(
        id.to_lowercase().as_str(),
        "footer"
            | "bottom"
            | "sidebar"
            | "aside"
            | "nav"
            | "navbar"
            | "menu"
            | "header"
            | "masthead"
            | "breadcrumb"
            | "top"
            | "topleft"
            | "topLeft"
            | "leftnav"
            | "rightnav"
            | "sitemap"
            | "copyright"
            | "legal"
            | "disclaimer"
            | "branding"
    )
}

/// Visible text length of a subtree, early-exit at `cap`.
/// Used to size-gate negative skips: real junk is small;
/// a "negative" class on a huge container is a false hit.
pub fn text_size(el: ElementRef<'_>, cap: usize) -> usize {
    let mut total = 0usize;
    let mut stack = vec![el];
    while let Some(node) = stack.pop() {
        if total >= cap {
            return total;
        }
        for child in node.children() {
            match child.value() {
                scraper::Node::Text(t) => total += t.text.trim().len(),
                scraper::Node::Element(_) => {
                    if let Some(c) = ElementRef::wrap(child)
                        && !skip(c)
                    {
                        stack.push(c);
                    }
                }
                _ => {}
            }
        }
    }
    total
}

#[cfg(test)]
mod ad_slot_tests {
    use super::*;
    use scraper::Html;

    fn skipped(html: &str) -> bool {
        let document = Html::parse_fragment(html);
        let root = document.root_element();
        let el = root.children().filter_map(ElementRef::wrap).next().unwrap();
        skip(el)
    }

    #[test]
    fn an_inline_glossary_popup_is_not_read_into_the_sentence() {
        let html = Html::parse_fragment(
            r#"<p>shall be <span class="term-ref">subject<span class="term-tip"><span>subject</span><span>for the purpose of testing, means a person</span></span></span> to fines</p>"#,
        );
        let tip = html
            .select(&scraper::Selector::parse(".term-tip").unwrap())
            .next()
            .unwrap();
        assert!(skip(tip));
        let p = html
            .select(&scraper::Selector::parse("p").unwrap())
            .next()
            .unwrap();
        let opts = crate::extract::ExtractOptions::default();
        assert_eq!(
            crate::extract::inline::markdown(p, "https://example.com/", &opts).0,
            "shall be subject to fines"
        );
        // A tip callout is a block of the document, and a span that is not
        // nested in running text is not a popup.
        assert!(!skipped(r#"<div class="tip"><p>Run it twice.</p></div>"#));
        assert!(!skipped(r#"<span class="tooltip-host">Hover me</span>"#));
    }

    #[test]
    fn an_inactive_tab_panel_is_content() {
        assert!(!skipped(
            r#"<div role="tabpanel" aria-hidden="true" inert=""><table><tr><td>CPU time</td><td>$0.02</td></tr></table></div>"#
        ));
        assert!(!skipped(
            r#"<div role="tabpanel" hidden><p>Annual plans</p></div>"#
        ));
        assert!(!skipped(
            r#"<div role="tabpanel" style="display:none"><p>Annual plans</p></div>"#
        ));
        // Hidden content that is not a tab panel stays out, and so does a
        // carousel's clone of a slide.
        assert!(skipped(
            r#"<div aria-hidden="true"><p>Annual plans</p></div>"#
        ));
        assert!(skipped(
            r#"<div role="tabpanel" class="slick-slide slick-cloned" aria-hidden="true"><p>Slide</p></div>"#
        ));
    }

    // #288: ad slots never ship as content.
    #[test]
    fn ad_slots_are_skipped() {
        assert!(skipped(
            "<shreddit-ad-post><div>Deals are BACK, baby</div></shreddit-ad-post>"
        ));
        assert!(skipped(
            "<div class=\"ad-container\"><a>Learn More</a></div>"
        ));
        assert!(skipped("<div id=\"adsbygoogle\">x</div>"));
        assert!(skipped("<div class=\"sponsored-post\">Buy</div>"));
        // Ordinary content mentioning ads, or classes that merely
        // contain the letters, is not an ad slot.
        assert!(!skipped(
            "<div class=\"article-body\"><p>About ads</p></div>"
        ));
        assert!(!skipped("<div class=\"loads shadow\"><p>x</p></div>"));
        // An element with its own positive content token keeps its
        // content role.
        assert!(!skipped("<div class=\"content ad-container\">x</div>"));
        // Existing semantics untouched.
        assert!(skipped("<script>var x</script>"));
        assert!(skipped("<style>.x{}</style>"));
        assert!(!skipped("<p>hello</p>"));
    }
}
