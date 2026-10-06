//! Hacker News dedicated extractor.
//!
//! HN's comment tree is a nested <table> layout : the generic
//! pipeline renders it as pipe-table rows (truncating every comment
//! to 120 chars) or loses it to main-content scoring. HN's markup
//! has been frozen for 15+ years, so a dedicated extractor is the
//! honest god-tier path: full comment text, authors, ages, and
//! reply depth, token-efficient.
//!
//! Returns None for non-HN pages or non-item pages (listings flow
//! through generic DonSift + prose-table walking).

use scraper::{ElementRef, Html, Selector};

use super::{ContentKind, ExtractOptions, Extracted, inline};

/// Max comments rendered before truncating (threads can run deep).
const MAX_COMMENTS: usize = 150;

pub fn extract(html: &str, url: &str, opts: &ExtractOptions) -> Option<Extracted> {
    if opts.selector.is_some() || opts.toc || opts.section.is_some() || opts.must_contain.is_some()
    {
        return None;
    }
    let parsed = url::Url::parse(url).ok()?;
    let host = parsed.host_str()?;
    if host != "news.ycombinator.com" {
        return None;
    }
    if !parsed.path().starts_with("/item") {
        return None;
    }

    let doc = Html::parse_document(html);
    let comtr = Selector::parse("tr.athing.comtr").ok()?;
    let comments: Vec<ElementRef> = doc
        .select(&comtr)
        .filter(|tr| {
            // "noshow" rows are collapsed/deleted branches; "coll"
            // are collapsed-but-present. Keep coll, drop noshow.
            !tr.value().classes().any(|c| c == "noshow")
        })
        .collect();
    if !comments.is_empty() {
        return extract_thread(&doc, url, &comments, opts);
    }
    // Comment-permalink shape (item?id=<comment-id>): a single
    // fatitem table holding the comment itself, no comtr rows.
    extract_permalink(&doc, url, opts)
}

/// Comment permalink: `table.fatitem` with one comment (comhead +
/// div.commtext), plus "on: <story>" context. 2026 HN shape.
fn extract_permalink(doc: &Html, url: &str, opts: &ExtractOptions) -> Option<Extracted> {
    let fat = doc.select(&Selector::parse("table.fatitem").ok()?).next()?;
    let commtext = fat.select(&Selector::parse("div.commtext").ok()?).next()?;
    let text = inline::markdown(commtext, url, opts).0;
    if text.trim().is_empty() {
        return None;
    }
    let author = fat
        .select(&Selector::parse("a.hnuser").ok()?)
        .next()
        .map(|a| inline::plain(a))
        .unwrap_or_default();
    let age = fat
        .select(&Selector::parse("span.age").ok()?)
        .next()
        .map(|a| inline::plain(a))
        .unwrap_or_default();
    // Story context: <span class="onstory">on: <a>Story Title</a>
    let story = fat
        .select(&Selector::parse("span.onstory a").ok()?)
        .next()
        .map(|a| (inline::plain(a), a.value().attr("href").map(String::from)))
        .or_else(|| {
            // The fatitem may be a story submission itself: its own
            // title link IS the story.
            fat.select(&Selector::parse("td.title a").ok()?)
                .next()
                .map(|a| (inline::plain(a), a.value().attr("href").map(String::from)))
        });

    let mut md = String::new();
    let title = match &story {
        Some((t, _)) if !t.is_empty() => {
            md.push_str(&format!("# Comment on: {t}\n"));
            t.clone()
        }
        _ => {
            md.push_str("# HN comment\n");
            "HN comment".to_string()
        }
    };
    let mut byline = String::new();
    if !author.is_empty() {
        byline.push_str(&format!("**{author}**"));
    }
    if !age.is_empty() {
        if !byline.is_empty() {
            byline.push_str(" · ");
        }
        byline.push_str(&age);
    }
    if !byline.is_empty() {
        md.push_str(&format!("{byline}\n"));
    }
    if let Some((_, Some(href))) = &story
        && let Some(abs) = inline::absolutize(url, href)
    {
        md.push_str(&format!("thread: {abs}\n"));
    }
    md.push_str(&format!("{url}\n\n"));
    md.push_str(text.trim());
    md.push('\n');

    let total = md.len();
    let max_chars = opts.max_chars.unwrap_or(16_000).max(200);
    let (slice, next) = crate::extract::paginate_public(&md, opts.offset, max_chars);
    let tokens_est = slice.len() / 4;
    Some(Extracted {
        markdown: slice,
        title: Some(title),
        byline: (!author.is_empty()).then(|| author.clone()),
        published: None,
        site: Some("news.ycombinator.com".to_string()),
        total_chars: total,
        next_offset: next,
        blocks_total: 1,
        blocks_shown: 1,
        tokens_est,
        thin: total < 800,
        content_kind: ContentKind::Forum,
        lang: "en".to_string(),
        quality: 0.9,
        pdf_pages: None,
        images: Vec::new(),
        fingerprint: None,
        via: None,
        partial: None,
    })
}

#[derive(Clone)]
struct Comment {
    depth: usize,
    author: String,
    age: String,
    text: String,
}

fn extract_thread(
    doc: &Html,
    url: &str,
    comments: &[ElementRef<'_>],
    opts: &ExtractOptions,
) -> Option<Extracted> {
    // ── Story header ──
    let title = doc
        .select(
            &Selector::parse(
                "tr.athing.submission td.title a.title-link, tr.athing td.title:last-child a",
            )
            .ok()?,
        )
        .next()
        .map(|a| inline::plain(a))
        .filter(|t| !t.is_empty())
        .or_else(|| {
            doc.select(&Selector::parse("title").unwrap())
                .next()
                .map(|t| {
                    t.text()
                        .collect::<String>()
                        .trim_end_matches("| Hacker News")
                        .trim()
                        .to_string()
                })
        })?;

    let link = doc
        .select(&Selector::parse("tr.athing.submission a.title-link").ok()?)
        .next()
        .and_then(|a| a.value().attr("href"))
        .and_then(|h| inline::absolutize(url, h))
        .unwrap_or_else(|| url.to_string());

    // Subtext: points · author · age · comment count.
    let subtext = doc.select(&Selector::parse("td.subtext").ok()?).next();
    let points = subtext
        .and_then(|s| s.select(&Selector::parse("span.score").ok()?).next())
        .map(|p| inline::plain(p))
        .unwrap_or_default();
    let author = subtext
        .and_then(|s| s.select(&Selector::parse("a.hnuser").ok()?).next())
        .map(|a| inline::plain(a))
        .unwrap_or_default();
    let age = subtext
        .and_then(|s| s.select(&Selector::parse("span.age").ok()?).next())
        .map(|a| inline::plain(a))
        .unwrap_or_default();
    let comment_count = subtext
        .map(|s| {
            s.select(&Selector::parse("a").unwrap())
                .map(|a| inline::plain(a))
                .find(|t| t.ends_with("comments") || t.ends_with("comment"))
                .unwrap_or_default()
        })
        .unwrap_or_default();

    let mut md = format!("# {title}\n");
    let mut byline = String::new();
    if !points.is_empty() {
        byline.push_str(&points);
    }
    if !author.is_empty() {
        if !byline.is_empty() {
            byline.push_str(" · ");
        }
        byline.push_str(&format!("by {author}"));
    }
    if !age.is_empty() {
        if !byline.is_empty() {
            byline.push_str(" · ");
        }
        byline.push_str(&age);
    }
    if !comment_count.is_empty() {
        byline.push_str(&format!(" · {comment_count}"));
    }
    if !byline.is_empty() {
        md.push_str(&format!("{byline}\n"));
    }
    md.push_str(&format!("{link}\n\n"));

    // Self-post text (Ask HN etc.): .toptext.
    if let Some(op) = doc
        .select(&Selector::parse("div.toptext, td.toptext").ok()?)
        .next()
    {
        let t = inline::markdown(op, url, opts).0;
        if !t.is_empty() {
            md.push_str(&t);
            md.push_str("\n\n");
        }
    }

    // ── Comments ──
    let ind_sel = Selector::parse("td.ind img").ok()?;
    let head_user = Selector::parse("a.hnuser").ok()?;
    let head_age = Selector::parse("span.age").ok()?;
    let commtext = Selector::parse(".commtext").ok()?;

    let mut parsed_comments: Vec<Comment> = Vec::with_capacity(comments.len());
    for tr in comments.iter().take(MAX_COMMENTS) {
        let depth = tr
            .select(&ind_sel)
            .next()
            .and_then(|img| img.value().attr("width"))
            .and_then(|w| w.parse::<usize>().ok())
            .map(|w| w / 40)
            .unwrap_or(0);
        // The comhead (author/age) and commtext live in the row's
        // default td; selecting within the tr is enough.
        let author = tr
            .select(&head_user)
            .next()
            .map(|a| inline::plain(a))
            .unwrap_or_else(|| "[deleted]".into());
        let age = tr
            .select(&head_age)
            .next()
            .map(|a| inline::plain(a))
            .unwrap_or_default();
        let text = tr
            .select(&commtext)
            .next()
            .map(|c| inline::markdown(c, url, opts).0)
            .unwrap_or_default();
        let text = text.trim().to_string();
        if text.is_empty() {
            continue; // collapsed / deleted
        }
        parsed_comments.push(Comment {
            depth,
            author,
            age,
            text,
        });
    }

    // Focus filter: a 700-comment thread with a focus query must
    // surface the RELEVANT comments, not the first N. Keep comments
    // matching any query term; no matches → full thread + notice
    // (same contract as the generic pipeline).
    let mut focus_missed = false;
    if let Some(q) = opts.focus.as_ref().filter(|q| !q.trim().is_empty()) {
        let terms: Vec<String> = q
            .to_lowercase()
            .split_whitespace()
            .map(String::from)
            .collect();
        let matched: Vec<Comment> = parsed_comments
            .iter()
            .filter(|c| {
                let hay = c.text.to_lowercase();
                terms.iter().any(|t| hay.contains(t))
            })
            .cloned()
            .collect();
        if matched.is_empty() {
            focus_missed = true;
        } else {
            let kept = matched.len();
            md.push_str(&format!(
                "*(focus \"{}\": showing {kept} of {} comments)*\n\n",
                q,
                parsed_comments.len()
            ));
            parsed_comments = matched;
        }
    }

    if !parsed_comments.is_empty() {
        md.push_str("## Discussion\n\n");
        for c in &parsed_comments {
            let indent = "  ".repeat(c.depth.min(12));
            let mut header = format!("{}**{}**", indent, c.author);
            if !c.age.is_empty() {
                header.push_str(&format!(" · {}", c.age));
            }
            md.push_str(&header);
            md.push_str("\n\n");
            // Indent continuation lines to preserve thread shape.
            for para in c.text.split("\n") {
                let p = para.trim();
                if !p.is_empty() {
                    md.push_str(&format!("{}{p}\n\n", indent));
                }
            }
        }
    }

    if comments.len() > MAX_COMMENTS {
        md.push_str(&format!(
            "*(thread truncated at {MAX_COMMENTS} comments of {})*\n",
            comments.len()
        ));
    }

    if focus_missed && let Some(q) = &opts.focus {
        md = format!("*(focus \"{q}\": no matches : showing full thread)*\n\n{md}");
    }

    let total = md.len();
    let (slice, next) = paginate(&md, opts);
    let tokens_est = slice.len() / 4;
    Some(Extracted {
        markdown: slice,
        title: Some(title),
        byline: (!author.is_empty()).then(|| author.clone()),
        published: None,
        site: Some("news.ycombinator.com".to_string()),
        total_chars: total,
        next_offset: next,
        blocks_total: parsed_comments.len(),
        blocks_shown: parsed_comments.len(),
        tokens_est,
        thin: parsed_comments.is_empty() && total < 800,
        content_kind: ContentKind::Forum,
        lang: "en".to_string(),
        quality: 0.95,
        pdf_pages: None,
        images: Vec::new(),
        fingerprint: None,
        via: None,
        partial: None,
    })
}

fn paginate(full: &str, opts: &ExtractOptions) -> (String, Option<usize>) {
    let max_chars = opts.max_chars.unwrap_or(16_000).max(200);
    crate::extract::paginate_public(full, opts.offset, max_chars)
}

#[cfg(test)]
mod tests {
    use super::*;

    const THREAD: &str = r##"<html><body><center>
    <table id="hnmain">
      <tr><td bgcolor="#ff6600">masthead links</td></tr>
      <tr><td>
        <table class="itemlist">
          <tr class="athing submission" id="123">
            <td class="title">1.</td>
            <td class="title"><a class="title-link" href="https://example.com/post">Example Discussion Post</a></td>
          </tr>
          <tr><td class="subtext">
            <span class="subline"><span class="score" id="score_123">542 points</span> by <a href="user?id=alice" class="hnuser">alice</a> <span class="age"><a href="item?id=123">6 hours ago</a></span> | <a href="item?id=123">128&nbsp;comments</a></span>
          </td></tr>
        </table>
        <br><br>
        <table class="comment-tree">
          <tr class="athing comtr" id="c1"><td><table><tr>
            <td class="ind"><img src="s.gif" height="1" width="0"></td>
            <td valign="top" class="default"><div class="comment">
              <div class="comhead"><a href="user?id=bob" class="hnuser">bob</a> <span class="age"><a href="item?id=c1">5 hours ago</a></span></div>
              <div class="comment"><span class="commtext c00">First comment with a full paragraph of substance. This is the complete text of the comment, well over one hundred and twenty characters so the old table-cell truncation would have destroyed it entirely and the agent would never see the end of this sentence at all.</span></div>
            </div></td>
          </tr></table></td></tr>
          <tr class="athing comtr" id="c2"><td><table><tr>
            <td class="ind"><img src="s.gif" height="1" width="40"></td>
            <td valign="top" class="default"><div class="comment">
              <div class="comhead"><a href="user?id=carol" class="hnuser">carol</a> <span class="age"><a href="item?id=c2">4 hours ago</a></span></div>
              <div class="comment"><span class="commtext c00"><p>Reply at depth one.</p><p>Second paragraph of the reply.</p></span></div>
            </div></td>
          </tr></table></td></tr>
        </table>
      </td></tr>
    </table>
    </center></body></html>"##;

    #[test]
    fn extracts_full_comment_text() {
        let ex = extract(
            THREAD,
            "https://news.ycombinator.com/item?id=123",
            &ExtractOptions::default(),
        )
        .expect("thread extracts");
        assert!(
            ex.markdown.contains("end of this sentence"),
            "{}",
            ex.markdown
        );
        // The 120-char truncation would have cut here:
        assert!(ex.markdown.contains("destroyed it entirely"));
        assert!(ex.markdown.contains("**bob** · 5 hours ago"));
        assert!(ex.markdown.contains("**carol**"));
        assert!(ex.markdown.contains("Second paragraph of the reply."));
        assert_eq!(ex.content_kind, ContentKind::Forum);
        assert!(!ex.thin);
    }

    #[test]
    fn reply_depth_is_indented() {
        let ex = extract(
            THREAD,
            "https://news.ycombinator.com/item?id=123",
            &ExtractOptions::default(),
        )
        .unwrap();
        // Carol is at depth 1 → two-space indent on her header.
        assert!(ex.markdown.contains("\n  **carol**"), "{}", ex.markdown);
    }

    #[test]
    fn story_header_rendered() {
        let ex = extract(
            THREAD,
            "https://news.ycombinator.com/item?id=123",
            &ExtractOptions::default(),
        )
        .unwrap();
        assert!(ex.markdown.contains("# Example Discussion Post"));
        assert!(ex.markdown.contains("542 points"));
        assert!(ex.markdown.contains("https://example.com/post"));
    }

    #[test]
    fn non_item_page_falls_through() {
        assert!(
            extract(
                THREAD,
                "https://news.ycombinator.com/",
                &ExtractOptions::default()
            )
            .is_none()
        );
        assert!(
            extract(
                THREAD,
                "https://example.com/item?id=1",
                &ExtractOptions::default()
            )
            .is_none()
        );
    }
}
