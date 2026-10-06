//! Reddit SSR (shreddit) adapter: the server-rendered www pages
//! restructured into the same cards the `.json` adapter produces.
//!
//! Where the `.json` endpoints refuse (reddit blocks them per-IP
//! while still serving the page), the fetch ladder lands on the
//! server-rendered page; this adapter makes that landing produce
//! the clean card instead of generic chrome: threads with the
//! comment tree, listings, subreddit about pages (header attrs +
//! rules) and wiki pages (full markdown + revision provenance).
//!
//! Every renderer keys on shreddit's own custom elements and
//! returns `None` on anything unrecognized : generic DonSift stays
//! the fallback.

use scraper::{ElementRef, Html, Selector};

use super::reddit_json::{
    MAX_COMMENTS, MAX_DEPTH, comment_head, epoch_from_iso, listing_line, thread_head,
};
use crate::extract::{ContentKind, ExtractOptions, Extracted, PartialContent, inline};

const MAX_ITEMS: usize = 40;

pub fn extract(html: &str, url: &str, opts: &ExtractOptions) -> Option<Extracted> {
    if opts.selector.is_some() {
        return None;
    }
    let u = url::Url::parse(url).ok()?;
    let host = u.host_str()?;
    if !crate::adapters::is_reddit_host(host) {
        return None;
    }
    // Cheap gate before parsing half a megabyte: every shreddit
    // surface declares itself with custom elements.
    if !html.contains("<shreddit-") {
        return None;
    }
    let segs: Vec<&str> = u.path().split('/').filter(|s| !s.is_empty()).collect();
    let doc = Html::parse_document(html);

    let rendered = if segs.contains(&"comments") {
        render_thread(&doc, &segs)
            .map(|(md, title, n, partial)| (md, title, ContentKind::Forum, n, partial))
    } else if segs.len() >= 3 && segs[0] == "r" && segs[2] == "wiki" {
        render_wiki(&doc).map(|(md, title)| (md, title, ContentKind::Article, 1, None))
    } else if segs.len() == 3 && segs[0] == "r" && segs[2] == "about" {
        render_about(&doc).map(|(md, title, n)| (md, title, ContentKind::Article, n, None))
    } else {
        // Listings, and any other page carrying posts (search
        // results, a ghost-rendered profile feed). Profile pages
        // as served at tier 1 carry their pinned/recent items and
        // stream the rest; the card keys on the user, not the
        // feed item's "u_<name>" prefixed label.
        render_listing(&doc, html).map(|(md, mut title, n)| {
            if matches!(segs.first(), Some(&"user") | Some(&"u"))
                && let Some(name) = segs.get(1)
            {
                title = format!("u/{name}");
            }
            (
                md,
                title,
                ContentKind::Listing,
                n,
                html.contains("/svc/shreddit/community-more-posts")
                    .then_some(PartialContent {
                        reason: "additional posts require client-side loading",
                        items_found: n,
                        items_total: None,
                    }),
            )
        })
    };
    let (md, title, kind, blocks, partial) = rendered?;

    let total = md.len();
    let max = opts.max_chars.unwrap_or(16_000).max(200);
    let (slice, next) = crate::extract::paginate_public(&md, opts.offset, max);
    Some(Extracted {
        tokens_est: slice.len() / 4,
        markdown: slice,
        title: Some(title),
        byline: None,
        published: None,
        site: Some("reddit".to_string()),
        total_chars: total,
        next_offset: next,
        blocks_total: blocks,
        blocks_shown: blocks,
        thin: false,
        content_kind: kind,
        lang: "en".to_string(),
        quality: 0.9,
        pdf_pages: None,
        images: Vec::new(),
        fingerprint: None,
        via: Some("adapter:reddit-html"),
        partial,
    })
}

// ── Thread ────────────────────────────────────────────────────

fn render_thread(
    doc: &Html,
    segs: &[&str],
) -> Option<(String, String, usize, Option<PartialContent>)> {
    let post_sel = Selector::parse("shreddit-post").ok()?;
    // Prefer the post the URL names (a thread page can carry
    // sidebar/related posts too).
    let want = segs
        .iter()
        .position(|s| *s == "comments")
        .and_then(|i| segs.get(i + 1))
        .map(|id| format!("t3_{id}"));
    let post = doc
        .select(&post_sel)
        .find(|p| {
            want.as_deref()
                .is_some_and(|w| p.value().attr("id") == Some(w))
        })
        .or_else(|| doc.select(&post_sel).next())?;

    let title_sel = Selector::parse("h1[slot='title']").ok()?;
    let title = post
        .select(&title_sel)
        .next()
        .map(text_of)
        .filter(|t| !t.is_empty())
        .or_else(|| post.value().attr("post-title").map(String::from))?;
    let author = post.value().attr("author").unwrap_or("[deleted]");
    let score = post
        .value()
        .attr("score")
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let created = post
        .value()
        .attr("created-timestamp")
        .and_then(epoch_from_iso);
    let comments_n = post
        .value()
        .attr("comment-count")
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let sub = post
        .value()
        .attr("subreddit-prefixed-name")
        .unwrap_or("reddit");

    let mut md = thread_head(
        &title,
        author,
        score,
        created,
        comments_n,
        sub.trim_start_matches("r/"),
        &[],
    );
    if let Some(flair) = post
        .value()
        .attr("post-flair-text")
        .filter(|f| !f.is_empty())
    {
        md.push_str(&format!("*flair: {flair}*\n\n"));
    }

    // Body or link, mirroring the `.json` card.
    let body_sel = Selector::parse("[slot='text-body']").ok()?;
    let body_md = post
        .select(&body_sel)
        .next()
        .map(|b| {
            inline::markdown(b, "https://www.reddit.com", &link_opts())
                .0
                .trim()
                .to_string()
        })
        .filter(|m| !m.is_empty());
    if let Some(b) = body_md {
        md.push_str(&b);
        md.push_str("\n\n---\n\n");
    } else if let Some(href) = post.value().attr("content-href")
        && !href.contains("reddit.com")
    {
        md.push_str(&format!("→ {href}\n\n---\n\n"));
    }

    let mut rendered = 0usize;
    let comment_sel = Selector::parse("shreddit-comment").ok()?;
    for c in doc.select(&comment_sel).collect::<Vec<_>>() {
        if nearest_comment(&c).is_none() {
            render_comment(c, 0, &mut md, &mut rendered);
        }
    }
    if comments_n > 0 && rendered < comments_n as usize {
        md.push_str(&format!(
            "*(showing {rendered} of {comments_n} comments)*\n"
        ));
    }
    let total = usize::try_from(comments_n).ok();
    let partial = total.filter(|n| rendered < *n).map(|n| PartialContent {
        reason: "additional comments require client-side loading",
        items_found: rendered,
        items_total: Some(n),
    });
    Some((md, title, rendered, partial))
}

fn render_comment(el: ElementRef, depth: usize, md: &mut String, rendered: &mut usize) {
    if *rendered >= MAX_COMMENTS {
        return;
    }
    let author = el.value().attr("author").unwrap_or("[deleted]");
    let score = el
        .value()
        .attr("score")
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let created = el.value().attr("created").and_then(epoch_from_iso);
    let indent = "  ".repeat(depth.min(MAX_DEPTH));
    md.push_str(&comment_head(&indent, author, score, created, ""));
    let body = own_body(el)
        .map(|b| {
            inline::markdown(b, "https://www.reddit.com", &link_opts())
                .0
                .trim()
                .to_string()
        })
        .unwrap_or_default();
    if body.is_empty() {
        md.push_str(&format!("{indent}[removed]\n\n"));
    } else {
        for line in body.lines() {
            md.push_str(&format!("{indent}{line}\n"));
        }
        md.push('\n');
    }
    *rendered += 1;

    if let Some(n) = own_more_replies(el) {
        md.push_str(&format!(
            "{indent}*(+{n} more replies : deeper thread)*\n\n"
        ));
    }
    for child in child_comments(el) {
        render_comment(child, depth + 1, md, rendered);
    }
}

/// The comment's own body slot (its nested replies carry their own
/// `[slot='comment']` elements, so scope by the nearest
/// `shreddit-comment` ancestor).
fn own_body(el: ElementRef) -> Option<ElementRef> {
    let sel = Selector::parse("[slot='comment']").ok()?;
    el.select(&sel)
        .find(|b| nearest_comment(b).map(|a| a.id()) == Some(el.id()))
}

fn nearest_comment<'a>(el: &ElementRef<'a>) -> Option<ElementRef<'a>> {
    el.ancestors()
        .filter_map(ElementRef::wrap)
        .find(|a| a.value().name() == "shreddit-comment")
}

fn own_more_replies(el: ElementRef) -> Option<i64> {
    let sel = Selector::parse("faceplate-tracker[noun='more_replies'] faceplate-number").ok()?;
    el.select(&sel)
        .find(|t| nearest_comment(t).map(|a| a.id()) == Some(el.id()))
        .and_then(|t| t.value().attr("number").and_then(|n| n.parse().ok()))
}

fn child_comments(el: ElementRef) -> Vec<ElementRef> {
    let Ok(sel) = Selector::parse("shreddit-comment") else {
        return Vec::new();
    };
    el.select(&sel)
        .filter(|c| nearest_comment(c).map(|a| a.id()) == Some(el.id()))
        .collect()
}

// ── Listing ───────────────────────────────────────────────────

fn render_listing(doc: &Html, html: &str) -> Option<(String, String, usize)> {
    let sel = Selector::parse("shreddit-post").ok()?;
    let posts: Vec<ElementRef> = doc.select(&sel).take(MAX_ITEMS).collect();
    if posts.is_empty() {
        return None;
    }
    let mut md = String::new();
    let mut n = 0usize;
    let mut sub = String::new();
    for p in &posts {
        let title = p.value().attr("post-title").unwrap_or("");
        if title.is_empty() {
            continue;
        }
        if sub.is_empty()
            && let Some(s) = p.value().attr("subreddit-prefixed-name")
        {
            sub = s.to_string();
        }
        let domain = p.value().attr("domain").unwrap_or("?");
        let score = p
            .value()
            .attr("score")
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        let author = p.value().attr("author").unwrap_or("[deleted]");
        let created = p.value().attr("created-timestamp").and_then(epoch_from_iso);
        let comments = p
            .value()
            .attr("comment-count")
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        let flair = p
            .value()
            .attr("post-flair-text")
            .filter(|f| !f.is_empty())
            .map(|f| format!(" · {f}"))
            .unwrap_or_default();
        n += 1;
        md.push_str(&listing_line(
            n, "", title, domain, score, author, created, comments, &flair,
        ));
    }
    if n == 0 {
        return None;
    }
    // The feed cut is reddit's own marker: further posts stream in
    // client-side.
    if html.contains("/svc/shreddit/community-more-posts") {
        md.push_str(&format!(
            "*(showing {n} server-rendered posts : reddit loads the rest with JavaScript)*\n"
        ));
    }
    Some((
        md,
        if sub.is_empty() {
            "reddit".to_string()
        } else {
            sub
        },
        n,
    ))
}

// ── About ─────────────────────────────────────────────────────

fn render_about(doc: &Html) -> Option<(String, String, usize)> {
    let hdr_sel = Selector::parse("shreddit-subreddit-header").ok()?;
    let hdr = doc.select(&hdr_sel).next()?;
    let pref = hdr.value().attr("prefixed-name").unwrap_or("");
    let name = hdr.value().attr("name").unwrap_or("");
    if pref.is_empty() && name.is_empty() {
        return None;
    }
    let label = if pref.is_empty() {
        format!("r/{name}")
    } else {
        pref.to_string()
    };
    let mut md = format!("# {label}\n");
    if let Some(dn) = hdr.value().attr("display-name")
        && !dn.is_empty()
    {
        md.push_str(&format!("{dn}\n"));
    }
    let mut facts: Vec<String> = Vec::new();
    if let Some(n) = hdr
        .value()
        .attr("weekly-active-users")
        .and_then(|v| v.parse::<u64>().ok())
    {
        facts.push(format!("{} weekly active", crate::adapters::human_count(n)));
    }
    if let Some(n) = hdr
        .value()
        .attr("weekly-contributions")
        .and_then(|v| v.parse::<u64>().ok())
    {
        facts.push(format!(
            "{} weekly contributions",
            crate::adapters::human_count(n)
        ));
    }
    if !facts.is_empty() {
        md.push_str(&format!("{}\n", facts.join(" · ")));
    }
    if let Some(desc) = hdr.value().attr("description")
        && !desc.is_empty()
    {
        md.push_str(&format!("\n{desc}\n"));
    }

    // Rules: each item pairs its summary (the tracker) with the
    // expandable description that follows it in the same <details>.
    let rule_sel = Selector::parse("faceplate-tracker[source='rules_widget']").ok()?;
    let h2_sel = Selector::parse("h2").ok()?;
    let desc_sel = Selector::parse("div[id$='post-rtjson-content']").ok()?;
    let mut rules = 0usize;
    let mut section = String::new();
    for r in doc.select(&rule_sel) {
        let title = r.select(&h2_sel).next().map(text_of).unwrap_or_default();
        if title.is_empty() {
            continue;
        }
        rules += 1;
        section.push_str(&format!("{rules}. {title}"));
        if let Some(desc) = r
            .ancestors()
            .filter_map(ElementRef::wrap)
            .find(|a| a.value().name() == "details")
            .and_then(|d| d.select(&desc_sel).next())
        {
            let d: String = text_of(desc)
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");
            let d: String = d.chars().take(240).collect();
            if !d.is_empty() {
                section.push_str(&format!(" : {d}"));
            }
        }
        section.push('\n');
    }
    if rules > 0 {
        md.push_str(&format!("\n## Rules\n{section}"));
    }
    Some((md, label, 1 + rules))
}

// ── Wiki ──────────────────────────────────────────────────────

fn render_wiki(doc: &Html) -> Option<(String, String)> {
    let body_sel = Selector::parse("div.md.wiki").ok()?;
    let body = doc.select(&body_sel).next()?;
    // A wiki page is a whole document: headings and block structure
    // survive through the shared block walker.
    let body_md =
        super::wiki_infobox::render_markdown_blocks(body, "https://www.reddit.com", &link_opts());
    if body_md.trim().is_empty() {
        return None;
    }
    let title = Selector::parse("shreddit-title")
        .ok()
        .and_then(|s| doc.select(&s).next())
        .and_then(|t| t.value().attr("title").map(String::from))
        .filter(|t| !t.is_empty())
        .unwrap_or_else(|| "Wiki".to_string());
    let mut md = String::new();
    if let Some((author, date)) = last_revised(body) {
        md.push_str(&format!("*(last revised {date} by u/{author})*\n\n"));
    }
    md.push_str(body_md.trim());
    md.push('\n');
    Some((md, title))
}

/// "Last revised by <user> <timeago ts=...>": the stamp sits after
/// the wiki body, outside it, in the same wrapper.
fn last_revised(body: ElementRef) -> Option<(String, String)> {
    let ta_sel = Selector::parse("faceplate-timeago[ts]").ok()?;
    let container = body
        .ancestors()
        .filter_map(ElementRef::wrap)
        .find(|a| a.select(&ta_sel).next().is_some())?;
    let t = container
        .select(&ta_sel)
        .find(|t| !t.ancestors().any(|a| a.id() == body.id()))?;
    let ts = t.value().attr("ts")?;
    let date: String = ts.chars().take(10).collect();
    let author = t
        .prev_siblings()
        .filter_map(ElementRef::wrap)
        .next()
        .map(text_of)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "?".to_string());
    Some((author, date))
}

// ── helpers ───────────────────────────────────────────────────

fn text_of(el: ElementRef) -> String {
    inline::visible_text_raw(el).trim().to_string()
}

/// Reddit bodies are markdown the author wrote (links included):
/// keep URLs, matching the `.json` adapter's verbatim bodies.
fn link_opts() -> ExtractOptions {
    ExtractOptions {
        include_links: true,
        ..ExtractOptions::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts() -> ExtractOptions {
        ExtractOptions::default()
    }

    const THREAD: &str = r#"<html><body>
      <shreddit-post id="t3_abc123" permalink="/r/rust/comments/abc123/title_here/" score="150"
          comment-count="36" created-timestamp="2026-09-22T19:34:03.310000+0000"
          subreddit-prefixed-name="r/rust" author="alice" post-type="text"
          post-title="A thread title" content-href="https://www.reddit.com/r/rust/comments/abc123/title_here/">
        <h1 id="post-title-t3_abc123" slot="title">A thread title</h1>
        <div slot="text-body"><div id="t3_abc123-post-rtjson-content" class="md"><p>Body <strong>text</strong> here.</p></div></div>
      </shreddit-post>
      <shreddit-comment-tree>
        <shreddit-comment author="bob" score="79" depth="0" thingId="t1_c1" created="2026-09-22T19:44:04.957000+0000" permalink="/r/rust/comments/abc123/comment/c1/">
          <div class="md" id="t1_c1-comment-rtjson-content" slot="comment"><div id="t1_c1-post-rtjson-content"><p>First comment.</p></div></div>
          <shreddit-comment author="carol" score="12" depth="1" thingId="t1_c2" created="2026-09-22T20:00:00.000000+0000" permalink="/r/rust/comments/abc123/comment/c2/">
            <div class="md" id="t1_c2-comment-rtjson-content" slot="comment"><div id="t1_c2-post-rtjson-content"><p>A reply.</p></div></div>
          </shreddit-comment>
          <span><button><faceplate-tracker source="post_detail" action="click" noun="more_replies"><faceplate-number number="27"></faceplate-number> more replies </faceplate-tracker></button></span>
        </shreddit-comment>
        <shreddit-comment author="dave" score="-2" depth="0" thingId="t1_c3" created="2026-09-22T21:00:00.000000+0000" permalink="/r/rust/comments/abc123/comment/c3/">
          <div class="md" id="t1_c3-comment-rtjson-content" slot="comment"></div>
        </shreddit-comment>
      </shreddit-comment-tree>
    </body></html>"#;

    #[test]
    fn thread_renders_with_the_comment_tree() {
        let ex = extract(
            THREAD,
            "https://www.reddit.com/r/rust/comments/abc123/title_here/",
            &opts(),
        )
        .expect("thread");
        assert_eq!(ex.via, Some("adapter:reddit-html"));
        assert!(ex.markdown.contains("# A thread title"));
        assert!(ex.markdown.contains("u/alice · 150 pts"));
        assert!(ex.markdown.contains("36 comments"));
        assert!(ex.markdown.contains("Body **text** here."));
        assert!(ex.markdown.contains("**u/bob** · 79 pts"));
        // Nested reply indented once.
        assert!(
            ex.markdown.contains("  **u/carol** · 12 pts"),
            "{}",
            ex.markdown
        );
        assert!(ex.markdown.contains("First comment."));
        assert!(ex.markdown.contains("A reply."));
        // Hidden replies marker.
        assert!(ex.markdown.contains("*(+27 more replies : deeper thread)*"));
        // An empty body reads as removed, never blank.
        assert!(ex.markdown.contains("**u/dave** · -2 pts"));
        assert!(ex.markdown.contains("[removed]"));
        // The SSR carries a subset: say so.
        assert!(ex.markdown.contains("*(showing 3 of 36 comments)*"));
        assert_eq!(ex.content_kind, ContentKind::Forum);
    }

    const LISTING: &str = r#"<html><body>
      <shreddit-post id="t3_a1" post-title="No More Code Dumps" author="matthieum" score="1545"
        comment-count="213" created-timestamp="2026-09-19T13:59:48.202000+0000"
        domain="self.rust" subreddit-prefixed-name="r/rust" permalink="/r/rust/comments/1wkmzun/no_more_code_dumps/"></shreddit-post>
      <shreddit-post id="t3_a2" post-title="Google's Binder" author="bob" score="99"
        comment-count="12" created-timestamp="2026-09-18T10:00:00.000000+0000"
        domain="lwn.net" post-flair-text="news" subreddit-prefixed-name="r/rust" permalink="/r/rust/comments/x/y/"></shreddit-post>
      <faceplate-partial src="/svc/shreddit/community-more-posts/top/"></faceplate-partial>
    </body></html>"#;

    #[test]
    fn listing_renders_with_the_feed_cut_note() {
        let ex = extract(
            LISTING,
            "https://www.reddit.com/r/rust/top/?t=week",
            &opts(),
        )
        .expect("listing");
        assert_eq!(ex.via, Some("adapter:reddit-html"));
        assert_eq!(ex.title.as_deref(), Some("r/rust"));
        assert!(
            ex.markdown
                .contains("1. **No More Code Dumps** (self.rust) · 1545 pts · u/matthieum"),
            "{}",
            ex.markdown
        );
        assert!(ex.markdown.contains("213 comments"));
        assert!(ex.markdown.contains("· news"));
        assert!(ex.markdown.contains(
            "*(showing 2 server-rendered posts : reddit loads the rest with JavaScript)*"
        ));
        assert_eq!(ex.content_kind, ContentKind::Listing);
    }

    // Profile feeds key on the user, not the item's "u_<name>"
    // prefixed label.
    #[test]
    fn profile_listing_keys_on_the_user() {
        let ex =
            extract(LISTING, "https://www.reddit.com/user/spez/", &opts()).expect("profile items");
        assert_eq!(ex.title.as_deref(), Some("u/spez"));
    }

    const ABOUT: &str = r#"<html><body>
      <shreddit-subreddit-header name="rust" prefixed-name="r/rust" display-name="The Rust Programming Language"
        description="A place for all things related to the Rust programming language."
        weekly-active-users="140821" weekly-contributions="2458"></shreddit-subreddit-header>
      <details><summary><faceplate-tracker source="rules_widget" action="click" noun="rules"><li><span>1</span><h2>Observe our code of conduct</h2></li></faceplate-tracker></summary>
        <div><div class="md" id="-post-rtjson-content"><p>Strive to treat others with respect.</p></div></div></details>
      <details><summary><faceplate-tracker source="rules_widget" action="click" noun="rules"><li><span>2</span><h2>No code dumps</h2></li></faceplate-tracker></summary>
        <div><div class="md" id="-post-rtjson-content"><p>Share projects in the weekly thread.</p></div></div></details>
    </body></html>"#;

    #[test]
    fn about_renders_with_rules() {
        let ex = extract(ABOUT, "https://www.reddit.com/r/rust/about/", &opts()).expect("about");
        assert_eq!(ex.title.as_deref(), Some("r/rust"));
        assert!(ex.markdown.contains("# r/rust"));
        assert!(ex.markdown.contains("The Rust Programming Language"));
        assert!(ex.markdown.contains("140.8k weekly active"));
        assert!(ex.markdown.contains("2.5k weekly contributions"));
        assert!(ex.markdown.contains("## Rules"));
        assert!(
            ex.markdown
                .contains("1. Observe our code of conduct : Strive to treat others with respect."),
            "{}",
            ex.markdown
        );
        assert!(ex.markdown.contains("2. No code dumps"));
    }

    const WIKI: &str = r#"<html><body>
      <shreddit-title title="r/personalfinance Wiki: Your Guide to Financial Wellness"></shreddit-title>
      <div class="bg-neutral"><div class="wrapper">
        <!-- SC_OFF --><html><head></head><body><div class="md wiki">
          <h1 id="wiki_welcome">Welcome to the PF Wiki</h1>
          <p>Read this <a href="https://example.com/guide">basic advice</a> first.</p>
        </div></body></html><!-- SC_ON -->
        <script src="https://embed.reddit.com/widgets.js" defer></script>
        <hr>
        <span>Last revised by <a href="/user/dequeued/">dequeued</a> <faceplate-timeago ts="2023-04-21T22:25:34.000000+0000"></faceplate-timeago></span>
      </div></div>
    </body></html>"#;

    #[test]
    fn wiki_renders_body_and_revision() {
        let ex = extract(
            WIKI,
            "https://www.reddit.com/r/personalfinance/wiki/index",
            &opts(),
        )
        .expect("wiki");
        assert_eq!(
            ex.title.as_deref(),
            Some("r/personalfinance Wiki: Your Guide to Financial Wellness")
        );
        assert!(
            ex.markdown
                .contains("*(last revised 2023-04-21 by u/dequeued)*")
        );
        assert!(
            ex.markdown.contains("# Welcome to the PF Wiki"),
            "{}",
            ex.markdown
        );
        assert!(
            ex.markdown
                .contains("[basic advice](https://example.com/guide)")
        );
    }

    #[test]
    fn non_reddit_and_plain_pages_rejected() {
        assert!(
            extract(
                THREAD,
                "https://example.com/r/rust/comments/abc123/x/",
                &opts()
            )
            .is_none()
        );
        // Reddit host, but no shreddit surface (old-reddit markup).
        let old = "<html><body><div class=\"thing link\"></div></body></html>";
        assert!(extract(old, "https://www.reddit.com/r/rust/", &opts()).is_none());
        // A profile page as served has no posts: nothing to render.
        let shell = "<html><body><shreddit-app-attrs routeName=\"profile\"></shreddit-app-attrs></body></html>";
        assert!(extract(shell, "https://www.reddit.com/user/spez/", &opts()).is_none());
    }
}
