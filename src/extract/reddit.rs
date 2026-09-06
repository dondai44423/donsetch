//! Reddit dedicated extractor for old.reddit.com.
//!
//! Bypasses DonSift entirely : selects and formats reddit-
//! specific elements (posts, comments, vote buttons, tags)
//! into compact, token-efficient markdown. Handles subreddit
//! listings and comment threads with proper nesting.
//!
//! Returns `None` for non-reddit or unrecognized reddit pages
//! (search, user pages without .thing elements) : the caller
//! falls back to generic DonSift.

use scraper::{ElementRef, Html, Node, Selector};

use super::{ContentKind, ExtractOptions, Extracted, inline};

/// Entry point. Tries the reddit extractor; returns `None`
/// to signal "not reddit, or unrecognized : use generic".
pub fn extract(html: &str, url: &str, opts: &ExtractOptions) -> Option<Extracted> {
    // Respect explicit selectors and TOC : let DonSift handle.
    if opts.selector.is_some() || opts.toc {
        return None;
    }

    let parsed = url::Url::parse(url).ok()?;
    let host = parsed.host_str()?.to_string();
    if !host.ends_with("reddit.com") {
        return None;
    }

    let doc = Html::parse_document(html);
    let path = parsed.path();

    if path.contains("/comments/") {
        extract_thread(&doc, url, opts)
    } else if has_listing(&doc) {
        extract_listing(&doc, url, opts)
    } else {
        None // user page, search, etc. → fall through
    }
}

// ── Detection helpers ─────────────────────────────────────────

fn has_listing(doc: &Html) -> bool {
    Selector::parse("div.thing.link")
        .ok()
        .and_then(|s| doc.select(&s).next())
        .is_some()
}

fn extract_subreddit(url: &str, doc: &Html) -> String {
    if let Ok(u) = url::Url::parse(url)
        && let Some(name) = u
            .path()
            .strip_prefix("/r/")
            .and_then(|r| r.split('/').next())
        && !name.is_empty()
    {
        return name.to_string();
    }
    if let Some(title_el) = doc.select(&Selector::parse("title").unwrap()).next() {
        let title = title_el.text().collect::<String>();
        if let Some(name) = title.split(" - ").next() {
            let n = name.trim();
            if !n.is_empty() {
                return n.to_string();
            }
        }
    }
    "reddit".to_string()
}

// ── Subreddit listing ─────────────────────────────────────────

fn extract_listing(doc: &Html, url: &str, opts: &ExtractOptions) -> Option<Extracted> {
    let sel = Selector::parse("div.thing.link").ok()?;
    let links: Vec<ElementRef> = doc
        .select(&sel)
        .filter(|el| !el.value().classes().any(|c| c == "promoted"))
        .collect();

    if links.is_empty() {
        return None;
    }

    let subreddit = extract_subreddit(url, doc);
    let mut md = format!("# r/{subreddit}\n\n");

    let title_sel = Selector::parse("a.title").ok()?;
    let time_sel = Selector::parse("time.live-timestamp").ok()?;
    let body_sel = Selector::parse("div.usertext-body div.md").ok()?;

    let mut count = 0usize;
    for link in &links {
        let rank = link
            .value()
            .attr("data-rank")
            .and_then(|s| s.parse::<usize>().ok())
            .unwrap_or(count + 1);

        let title = link
            .select(&title_sel)
            .next()
            .map(inline::plain)
            .unwrap_or_default();
        if title.is_empty() {
            continue;
        }

        let domain = link.value().attr("data-domain").unwrap_or("?");
        let score = link.value().attr("data-score").unwrap_or("0");
        let author = link.value().attr("data-author").unwrap_or("[deleted]");
        let comments = link.value().attr("data-comments-count").unwrap_or("0");
        let nsfw = link.value().attr("data-nsfw") == Some("true");
        let stickied = link.value().classes().any(|c| c == "stickied");

        let time = link
            .select(&time_sel)
            .next()
            .map(|el| el.text().collect::<String>().trim().to_string())
            .unwrap_or_default();
        let time_abbr = abbreviate_time(&time);

        let score_n: i64 = score.parse().unwrap_or(0);
        let prefix = if stickied {
            "[sticky] "
        } else if nsfw {
            "[NSFW] "
        } else {
            ""
        };

        md.push_str(&format!(
            "{rank}. {prefix}**{title}** ({domain}) · {score_n} pts · u/{author} · {time_abbr} · {comments} comments\n\n"
        ));

        // Self-post body preview (if expanded in listing :
        // stickied/announcement posts often are).
        if let Some(body) = link.select(&body_sel).next() {
            let body_text = inline::plain(body);
            if !body_text.is_empty() {
                let preview: String = body_text.chars().take(200).collect();
                md.push_str(&format!("   > {preview}\n\n"));
            }
        }

        count += 1;
    }

    let total = md.len();
    let (slice, next) = paginate(&md, opts);
    Some(Extracted {
        markdown: slice,
        title: Some(format!("r/{subreddit}")),
        byline: None,
        published: None,
        site: Some("reddit".to_string()),
        total_chars: total,
        next_offset: next,
        blocks_total: count,
        blocks_shown: count,
        tokens_est: total / 4,
        thin: false,
        content_kind: ContentKind::Listing,
        lang: "en".to_string(),
        quality: 0.9,
        pdf_pages: None,
        images: Vec::new(),
        fingerprint: None,
        via: None,
    })
}

// ── Thread (post + comments) ──────────────────────────────────

fn extract_thread(doc: &Html, url: &str, opts: &ExtractOptions) -> Option<Extracted> {
    let subreddit = extract_subreddit(url, doc);

    // Find the post (first .thing.link).
    let link_sel = Selector::parse("div.thing.link").ok()?;
    let link = doc.select(&link_sel).next()?;

    let title_sel = Selector::parse("a.title").ok()?;
    let title = link
        .select(&title_sel)
        .next()
        .map(inline::plain)
        .unwrap_or_default();
    if title.is_empty() {
        return None;
    }

    let author = link.value().attr("data-author").unwrap_or("[deleted]");
    let score = link.value().attr("data-score").unwrap_or("0");
    let score_n: i64 = score.parse().unwrap_or(0);

    let time_sel = Selector::parse("time.live-timestamp").ok()?;
    let time = link
        .select(&time_sel)
        .next()
        .map(|el| el.text().collect::<String>().trim().to_string())
        .unwrap_or_default();
    let time_abbr = abbreviate_time(&time);

    let mut md = format!("# {title}\n");
    md.push_str(&format!(
        "u/{author} · {score_n} pts · {time_abbr} · r/{subreddit}\n\n"
    ));

    // Post body (self-posts have .usertext-body .md).
    let md_sel = Selector::parse("div.usertext-body div.md").ok()?;
    if let Some(body) = link.select(&md_sel).next() {
        let body_md = render_md(body, url, opts);
        if !body_md.is_empty() {
            md.push_str(&body_md);
            md.push_str("\n\n---\n\n");
        }
    }

    // Comments : walk the nested tree.
    let comment_sel = Selector::parse("div.thing.comment").ok()?;
    let sitetable_sel = Selector::parse("div.sitetable").ok()?;

    let mut comment_count = 0usize;
    if let Some(sitetable) = doc
        .select(&Selector::parse("div.commentarea").unwrap())
        .next()
        .and_then(|ca| ca.select(&sitetable_sel).next())
    {
        for comment in get_direct_things(sitetable) {
            if comment.value().classes().any(|c| c == "comment") {
                render_comment(&comment, url, 0, &mut md, opts, &mut comment_count);
            }
        }
    }

    let _ = comment_sel; // suppress unused warning
    let total = md.len();
    let (slice, next) = paginate(&md, opts);
    Some(Extracted {
        markdown: slice,
        title: Some(title),
        byline: Some(format!("u/{author}")),
        published: Some(time),
        site: Some("reddit".to_string()),
        total_chars: total,
        next_offset: next,
        blocks_total: comment_count,
        blocks_shown: comment_count,
        tokens_est: total / 4,
        thin: false,
        content_kind: ContentKind::Forum,
        lang: "en".to_string(),
        quality: 0.95,
        pdf_pages: None,
        images: Vec::new(),
        fingerprint: None,
        via: None,
    })
}

// ── Comment tree ──────────────────────────────────────────────

/// Direct child `div.thing` elements of a sitetable (not
/// descendants : old.reddit nests comments inside
/// `.child > .sitetable`, and scraper's `select` returns
/// all descendants, which would double-count).
fn get_direct_things(sitetable: ElementRef) -> Vec<ElementRef> {
    let mut things = Vec::new();
    for child in sitetable.children() {
        if let Some(el) = ElementRef::wrap(child)
            && el.value().name() == "div"
            && el.value().classes().any(|c| c == "thing")
        {
            things.push(el);
        }
    }
    things
}

fn render_comment(
    el: &ElementRef,
    url: &str,
    depth: usize,
    md: &mut String,
    opts: &ExtractOptions,
    count: &mut usize,
) {
    let indent = "  ".repeat(depth.min(6));

    let author = el.value().attr("data-author").unwrap_or("[deleted]");
    let is_deleted = author == "[deleted]" || author.is_empty();

    // Score: .score.unvoted title attr has the plain number.
    let score_sel = Selector::parse("span.score.unvoted").ok();
    let score_text = score_sel
        .and_then(|s| el.select(&s).next())
        .and_then(|e| e.value().attr("title"))
        .unwrap_or("0");
    let score_n: i64 = score_text.parse().unwrap_or(0);

    // Time.
    let time_sel = Selector::parse("time.live-timestamp").ok();
    let time = time_sel
        .and_then(|s| el.select(&s).next())
        .map(|e| e.text().collect::<String>().trim().to_string())
        .unwrap_or_default();
    let time_abbr = abbreviate_time(&time);

    // Body.
    let md_sel = Selector::parse("div.usertext-body div.md").ok();
    let body = md_sel.and_then(|s| el.select(&s).next());

    md.push_str(&format!(
        "{indent}**u/{author}** · {score_n} pts · {time_abbr}\n"
    ));

    if is_deleted {
        md.push_str(&format!("{indent}[removed]\n\n"));
        *count += 1;
        return;
    }
    let body_md = body.map_or_else(String::new, |b| render_md(b, url, opts));
    if body_md.is_empty() {
        md.push_str(&format!("{indent}[removed]\n\n"));
    } else {
        for line in body_md.lines() {
            md.push_str(&format!("{indent}{line}\n"));
        }
        md.push('\n');
    }
    *count += 1;

    // Nested replies: .child > .sitetable > .thing.comment
    let child_sel = Selector::parse("div.child").ok();
    let sitetable_sel = Selector::parse("div.sitetable").ok();
    if let Some(child_div) = child_sel.and_then(|s| el.select(&s).next())
        && let Some(sitetable) = sitetable_sel.and_then(|s| child_div.select(&s).next())
    {
        for nested in get_direct_things(sitetable) {
            if nested.value().classes().any(|c| c == "comment") {
                render_comment(&nested, url, depth + 1, md, opts, count);
            }
        }
    }
}

// ── Reddit .md content → markdown ──────────────────────────────

fn render_md(el: ElementRef, url: &str, opts: &ExtractOptions) -> String {
    // Reddit comments always need links : links to playgrounds,
    // docs, code are content, not navigation.
    let md_opts = ExtractOptions {
        include_links: true,
        ..opts.clone()
    };

    let mut out = String::new();

    for child in el.children() {
        match child.value() {
            Node::Text(t) => {
                let s = t.text.trim();
                if !s.is_empty() {
                    out.push_str(s);
                    out.push('\n');
                }
            }
            Node::Element(_) => {
                let Some(c) = ElementRef::wrap(child) else {
                    continue;
                };
                match c.value().name() {
                    "p" => {
                        let (m, _) = inline::markdown(c, url, &md_opts);
                        if !m.is_empty() {
                            out.push_str(&m);
                            out.push_str("\n\n");
                        }
                    }
                    "pre" => {
                        let code: String = c.text().collect::<Vec<_>>().join("");
                        let code = code.trim_matches('\n');
                        if !code.is_empty() {
                            let fence = super::render::code_fence(code);
                            out.push_str(&format!("{fence}\n{code}\n{fence}\n\n"));
                        }
                    }
                    "blockquote" => {
                        let inner = render_md(c, url, opts);
                        for line in inner.lines() {
                            if line.is_empty() {
                                out.push_str(">\n");
                            } else {
                                out.push_str(&format!("> {line}\n"));
                            }
                        }
                        out.push('\n');
                    }
                    "ul" => {
                        render_list(c, url, false, 0, &mut out, &md_opts);
                        out.push('\n');
                    }
                    "ol" => {
                        render_list(c, url, true, 0, &mut out, &md_opts);
                        out.push('\n');
                    }
                    "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                        let level = c.value().name().as_bytes()[1] - b'0';
                        let (m, _) = inline::markdown(c, url, &md_opts);
                        if !m.is_empty() {
                            out.push_str(&format!("{} {m}\n\n", "#".repeat(level as usize)));
                        }
                    }
                    "hr" => {
                        out.push_str("---\n\n");
                    }
                    // Unknown block: recurse for nested content.
                    "div" => {
                        let inner = render_md(c, url, opts);
                        if !inner.is_empty() {
                            out.push_str(&inner);
                        }
                    }
                    _ => {
                        let (m, _) = inline::markdown(c, url, &md_opts);
                        if !m.is_empty() {
                            out.push_str(&m);
                            out.push('\n');
                        }
                    }
                }
            }
            _ => {}
        }
    }

    out.trim_end().to_string()
}

fn render_list(
    el: ElementRef,
    url: &str,
    ordered: bool,
    depth: usize,
    out: &mut String,
    opts: &ExtractOptions,
) {
    let li_sel = Selector::parse("li").unwrap();
    for (i, li) in el.select(&li_sel).enumerate() {
        let prefix = if ordered {
            format!("{}. ", i + 1)
        } else {
            "- ".to_string()
        };
        let indent = "  ".repeat(depth);
        let (m, _) = inline::markdown(li, url, opts);
        if !m.is_empty() {
            out.push_str(&format!("{indent}{prefix}{m}\n"));
        }
        // Nested lists.
        let nested_sel = Selector::parse("ul, ol").unwrap();
        for nested in li.select(&nested_sel) {
            render_list(
                nested,
                url,
                nested.value().name() == "ol",
                depth + 1,
                out,
                opts,
            );
        }
    }
}

// ── Formatting helpers ────────────────────────────────────────

/// "4 days ago" → "4d", "1 hour ago" → "1h", "an hour ago" → "1h".
fn abbreviate_time(time: &str) -> String {
    let t = time.trim().to_lowercase();
    if t.is_empty() || t == "just now" {
        return "now".to_string();
    }
    let parts: Vec<&str> = t.split_whitespace().collect();
    if parts.len() >= 2 {
        let n = if parts[0] == "an" || parts[0] == "a" {
            "1"
        } else {
            parts[0]
        };
        let unit = parts[1].trim_end_matches('s');
        let abbr = match unit {
            "second" => "s",
            "minute" => "m",
            "hour" => "h",
            "day" => "d",
            "week" => "w",
            "month" => "mo",
            "year" => "y",
            _ => return t,
        };
        return format!("{n}{abbr}");
    }
    t
}

/// Apply caller's max_chars/offset to the reddit output.
fn paginate(full: &str, opts: &ExtractOptions) -> (String, Option<usize>) {
    let max = opts.max_chars.unwrap_or(16_000).max(200);
    let offset = opts.offset;

    // Char-safe slicing (reddit content can be unicode).
    let chars: Vec<char> = full.chars().collect();
    if offset >= chars.len() {
        return (String::new(), None);
    }
    let end = offset.saturating_add(max).min(chars.len());
    let slice: String = chars[offset..end].iter().collect();
    let next = if end < chars.len() { Some(end) } else { None };
    (slice, next)
}
