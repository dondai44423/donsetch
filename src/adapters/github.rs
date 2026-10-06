//! GitHub adapter: issues / PRs / releases / commits pages
//! restructured from server-rendered HTML : no auth, no API-key
//! rate jail. The generic pipeline mangles these (sidebars,
//! reaction rows, filter bars); the agent wants the thread.
//!
//! Every selector is defensive: a GitHub redesign makes one
//! `extract` arm return `None` → generic DonSift, never a
//! crash. Recognized pages render structured markdown and carry
//! `via=adapter:github-html`.

use scraper::{ElementRef, Html, Selector};

use crate::extract::{ContentKind, ExtractOptions, Extracted, inline};

const MAX_ITEMS: usize = 30;
const MAX_COMMENTS: usize = 60;

/// Entry point.
pub fn extract(html: &str, url: &str, opts: &ExtractOptions) -> Option<Extracted> {
    if opts.selector.is_some() {
        return None; // explicit selector = caller knows better
    }
    let u = url::Url::parse(url).ok()?;
    let host = u.host_str()?;
    if host != "github.com" && host != "www.github.com" {
        return None;
    }
    let doc = Html::parse_document(html);

    let segments: Vec<&str> = u.path().trim_matches('/').split('/').collect();
    let md = match segments.as_slice() {
        [_owner, _repo, "issues" | "pulls"] => render_issue_list(&doc),
        [owner, repo, "issues" | "pull", number] if number.chars().all(|c| c.is_ascii_digit()) => {
            render_thread(&doc, owner, repo, number)
        }
        [_owner, _repo, "releases"] => render_releases(&doc),
        // The canonical commits URL carries the ref
        // (/commits/master): the arm takes any trailing segments.
        [_owner, _repo, "commits", ..] => render_commits(&doc),
        _ => None,
    }?;

    let total = md.len();
    let max = opts.max_chars.unwrap_or(16_000).max(200);
    let (slice, next) = crate::extract::paginate_public(&md, opts.offset, max);
    Some(Extracted {
        tokens_est: slice.len() / 4,
        markdown: slice,
        title: doc_title(&doc),
        byline: None,
        published: None,
        site: Some("github".to_string()),
        total_chars: total,
        next_offset: next,
        blocks_total: 0,
        blocks_shown: 0,
        thin: false,
        content_kind: ContentKind::Forum,
        lang: "en".to_string(),
        quality: 0.9,
        pdf_pages: None,
        images: Vec::new(),
        fingerprint: None,
        via: Some("adapter:github-html"),
        partial: None,
    })
}

fn doc_title(doc: &Html) -> Option<String> {
    Selector::parse("title")
        .ok()
        .and_then(|s| doc.select(&s).next())
        .map(|t| t.text().collect::<String>().trim().to_string())
        .filter(|t| !t.is_empty())
}

fn text_of(el: ElementRef) -> String {
    // Visible text only: a script/style subtree inside the element is
    // source, not content (#288).
    inline::visible_text_raw(el).trim().to_string()
}

// ── Issue / PR list ───────────────────────────────────────────

fn render_issue_list(doc: &Html) -> Option<String> {
    // GitHub shipped a React list (2025+): CSS-module hashed
    // classnames, but stable data-testid hooks. The old
    // li.js-issue-row markup still exists on some surfaces :
    // try the new shape first, keep the old as fallback.
    if let Some(md) = render_issue_list_modern(doc) {
        return Some(md);
    }
    render_issue_list_legacy(doc)
}

fn render_issue_list_modern(doc: &Html) -> Option<String> {
    let link_sel = Selector::parse("a[data-testid='issue-pr-title-link']").ok()?;
    let state_sel = Selector::parse("[data-testid='list-row-state-icon'] svg").ok()?;
    let user_sel = Selector::parse("a[data-hovercard-type='user']").ok()?;
    let label_sel = Selector::parse("a[href*='label%3A']").ok()?;
    let time_sel = Selector::parse("relative-time").ok()?;

    let links: Vec<ElementRef> = doc.select(&link_sel).collect();
    if links.is_empty() {
        return None;
    }
    let mut md = String::from("# Issues\n\n");
    let mut n = 0;
    for link in links.iter().take(MAX_ITEMS) {
        let title = text_of(*link);
        if title.is_empty() {
            continue;
        }
        let href = link.value().attr("href").unwrap_or_default();
        let number = href.rsplit('/').next().unwrap_or("?").to_string();
        // Closest ancestor carrying the state icon = the row.
        let row = link
            .ancestors()
            .filter_map(ElementRef::wrap)
            .find(|a| a.select(&state_sel).next().is_some());
        let Some(row) = row else { continue };
        let closed = row
            .select(&state_sel)
            .next()
            .map(|s| {
                s.value().attr("class").is_some_and(|c| {
                    c.contains("issue-closed") || c.contains("pull-request-closed")
                })
            })
            .unwrap_or(false);
        let state = if closed { "closed" } else { "open" };
        let author = row
            .select(&user_sel)
            .next()
            .map(text_of)
            .unwrap_or_default();
        let date = row
            .select(&time_sel)
            .next()
            .map(|t| text_of(t).trim_start_matches("on ").to_string())
            .unwrap_or_default();
        // Label anchors carry an sr-only tooltip ("Area: …");
        // the token's first Text span is the short name.
        let text_span = Selector::parse("span[data-component='Text']").unwrap();
        let labels: Vec<String> = row
            .select(&label_sel)
            .map(|a| {
                a.select(&text_span)
                    .next()
                    .map(text_of)
                    .unwrap_or_else(|| text_of(a))
            })
            .filter(|l| !l.is_empty())
            .take(4)
            .collect();
        let label_str = if labels.is_empty() {
            String::new()
        } else {
            format!(" · [{}]", labels.join(", "))
        };
        let author_str = if author.is_empty() {
            String::new()
        } else {
            format!(" · u/{author}")
        };
        let date_str = if date.is_empty() {
            String::new()
        } else {
            format!(" · {date}")
        };
        n += 1;
        md.push_str(&format!(
            "{n}. **{title}** #{number} · {state}{author_str}{date_str}{label_str}\n"
        ));
    }
    if n == 0 {
        return None;
    }
    Some(md)
}

fn render_issue_list_legacy(doc: &Html) -> Option<String> {
    let row_sel = Selector::parse("li.js-issue-row, div.js-issue-row").ok()?;
    let rows: Vec<ElementRef> = doc.select(&row_sel).collect();
    if rows.is_empty() {
        return None;
    }
    let id_sel = Selector::parse("a.js-navigation-open, a.Link--primary").ok()?;
    let mut md = String::new();
    let mut n = 0;
    for row in rows.iter().take(MAX_ITEMS) {
        let Some(link) = row.select(&id_sel).next() else {
            continue;
        };
        let title = text_of(link);
        if title.is_empty() {
            continue;
        }
        let href = link.value().attr("href").unwrap_or_default();
        let number = href.rsplit('/').next().unwrap_or("?").to_string();
        let state_open = row.select(&open_state_sel()).next().is_some();
        let state = if state_open { "open" } else { "closed" };
        let label_sel = Selector::parse("a.Label--secondary, span.Label--secondary").ok()?;
        let labels: Vec<String> = row
            .select(&label_sel)
            .map(text_of)
            .filter(|l| !l.is_empty() && !l.chars().all(|c| c.is_ascii_digit()))
            .take(4)
            .collect();
        let label_str = if labels.is_empty() {
            String::new()
        } else {
            format!(" · [{}]", labels.join(", "))
        };
        let count_sel = Selector::parse("a[aria-label*='comment'], span.Counter").ok()?;
        let comments = row
            .select(&count_sel)
            .next()
            .map(|c| {
                c.value()
                    .attr("aria-label")
                    .map(String::from)
                    .unwrap_or_else(|| text_of(c))
            })
            .map(|a| a.split_whitespace().next().unwrap_or("0").to_string())
            .unwrap_or_else(|| "0".into());
        n += 1;
        md.push_str(&format!(
            "{n}. **{title}** #{number} · {state} · {comments} comments{label_str}\n"
        ));
    }
    if n == 0 {
        return None;
    }
    Some(format!("# Issues\n\n{md}"))
}

fn open_state_sel() -> Selector {
    // Open-issue octicon: svg.octicon-issue-opened (issues) or
    // octicon-git-pull-request (open PRs).
    Selector::parse("svg.octicon-issue-opened, svg.octicon-git-pull-request").unwrap()
}

// ── Issue / PR thread ─────────────────────────────────────────

fn render_thread(doc: &Html, owner: &str, repo: &str, number: &str) -> Option<String> {
    // New React issue page (2025+): title/state/author/date/body
    // are server-rendered with stable data-testids; comments
    // stream in via JS (skeleton placeholders in the SSR HTML).
    if let Some(md) = render_thread_modern(doc, owner, repo, number) {
        return Some(md);
    }
    render_thread_legacy(doc, owner, repo, number)
}

fn render_thread_modern(doc: &Html, owner: &str, repo: &str, number: &str) -> Option<String> {
    let title_sel = Selector::parse("bdi[data-testid='issue-title']").ok()?;
    let state_sel = Selector::parse("[data-testid='header-state']").ok()?;
    let author_sel = Selector::parse("a[data-testid='issue-body-header-author']").ok()?;
    let date_sel = Selector::parse("[data-testid='issue-body-header-link'] relative-time").ok()?;
    let body_sel = Selector::parse("[data-testid='issue-body'] .markdown-body").ok()?;
    let skeleton_sel = Selector::parse("[data-testid='comment-skeleton']").ok()?;

    let title = doc
        .select(&title_sel)
        .next()
        .map(text_of)
        .filter(|t| !t.is_empty())?;
    let state = doc
        .select(&state_sel)
        .next()
        .and_then(|s| s.value().attr("data-status"))
        .map(|st| {
            if st.contains("losed") {
                "closed"
            } else if st.contains("erged") {
                "merged"
            } else {
                "open"
            }
        })
        .unwrap_or("open");
    let author = doc
        .select(&author_sel)
        .next()
        .map(text_of)
        .unwrap_or_default();
    // SSR relative-time carries visible text, not a datetime attr.
    let date = doc
        .select(&date_sel)
        .next()
        .map(|t| {
            t.value()
                .attr("datetime")
                .map(|d| d.chars().take(10).collect::<String>())
                .unwrap_or_else(|| text_of(t).trim_start_matches("on ").to_string())
        })
        .unwrap_or_default();

    let mut md = format!("# {title}\n");
    md.push_str(&format!(
        "{owner}/{repo}#{number} · {state} · u/{author} · opened {date}\n\n"
    ));
    if let Some(body) = doc.select(&body_sel).next() {
        let body_md = crate::extract::inline::markdown(
            body,
            &format!("https://github.com/{owner}/{repo}"),
            &gh_inline_opts(),
        )
        .0;
        if !body_md.trim().is_empty() {
            md.push_str(body_md.trim());
            md.push_str("\n\n");
        }
    }
    // Comments stream via JS : say so instead of silently
    // omitting the discussion.
    if doc.select(&skeleton_sel).next().is_some() {
        md.push_str(
            "*(comments load dynamically : re-fetch with tier=2 to read the discussion)*\n",
        );
    }
    Some(md.trim_end().to_string())
}

fn render_thread_legacy(doc: &Html, owner: &str, repo: &str, number: &str) -> Option<String> {
    let title_sel = Selector::parse("bdi.js-issue-title").ok()?;
    let title = doc
        .select(&title_sel)
        .next()
        .map(text_of)
        .filter(|t| !t.is_empty())?;
    let state_open = doc
        .select(&Selector::parse("svg.octicon-issue-opened, svg.octicon-git-pull-request").ok()?)
        .next()
        .is_some()
        && doc
            .select(&Selector::parse("svg.octicon-issue-closed, svg.octicon-git-merge").ok()?)
            .next()
            .is_none();
    let author_sel = Selector::parse("a.author").ok()?;
    let author = doc
        .select(&author_sel)
        .next()
        .map(text_of)
        .unwrap_or_else(|| "?".into());
    let time_sel = Selector::parse("relative-time").ok()?;
    let opened = doc
        .select(&time_sel)
        .next()
        .and_then(|t| t.value().attr("datetime"))
        .map(|d| d.chars().take(10).collect::<String>())
        .unwrap_or_default();

    let state = if state_open { "open" } else { "closed" };
    let mut md = format!("# {title}\n");
    md.push_str(&format!(
        "{owner}/{repo}#{number} · {state} · u/{author} · opened {opened}\n\n"
    ));

    // Timeline: the main discussion items. GitHub server-renders
    // .timeline-comment containers (comment bodies in
    // td.comment-body / .edit-comment-hide).
    let body_sel = Selector::parse("div.timeline-comment, div.js-timeline-item").ok()?;
    let inner_sel =
        Selector::parse("td.comment-body, div.comment-body, div.edit-comment-hide").ok()?;
    let author_sel2 = Selector::parse("a.author").ok()?;
    let mut count = 0;
    for item in doc.select(&body_sel).take(MAX_COMMENTS + 2) {
        let who = item
            .select(&author_sel2)
            .next()
            .map(text_of)
            .unwrap_or_default();
        let Some(body) = item.select(&inner_sel).next() else {
            continue;
        };
        let body_md = crate::extract::inline::markdown(
            body,
            &format!("https://github.com/{owner}/{repo}"),
            &gh_inline_opts(),
        )
        .0;
        if body_md.trim().is_empty() {
            continue;
        }
        count += 1;
        let role = if count == 1 { " (opener)" } else { "" };
        md.push_str(&format!("**u/{who}**{role}\n\n"));
        md.push_str(body_md.trim());
        md.push_str("\n\n---\n\n");
    }
    if count == 0 {
        return None; // JS-only render (rare) → generic
    }
    Some(md.trim_end_matches("---\n\n").trim_end().to_string())
}

// ── Releases ──────────────────────────────────────────────────

fn render_releases(doc: &Html) -> Option<String> {
    // Release entries: the legacy section.release and the 2025+
    // server-rendered page (section[data-release-anchor]).
    let rel_sel =
        Selector::parse("section.release, div.release, section[data-release-anchor]").ok()?;
    // Both markups point the tag link at /releases/tag/...; the
    // legacy heading shape stays as a fallback.
    let tag_sel = Selector::parse("a[href*='/releases/tag/']").ok()?;
    let tag_fallback_sel = Selector::parse("h1 a, h2 a, .release-title a").ok()?;
    let date_sel = Selector::parse("relative-time").ok()?;
    let notes_sel = Selector::parse("div.markdown-body").ok()?;
    let mut md = String::from("# Releases\n\n");
    let mut n = 0;
    for rel in doc.select(&rel_sel).take(15) {
        let Some(tag_el) = rel
            .select(&tag_sel)
            .next()
            .or_else(|| rel.select(&tag_fallback_sel).next())
        else {
            continue;
        };
        let tag = text_of(tag_el);
        if tag.is_empty() {
            continue;
        }
        let date = rel
            .select(&date_sel)
            .next()
            .and_then(|t| t.value().attr("datetime"))
            .map(|d| d.chars().take(10).collect::<String>())
            .unwrap_or_default();
        n += 1;
        md.push_str(&format!("## {tag} : {date}\n\n"));
        if let Some(notes) = rel.select(&notes_sel).next() {
            let notes_md =
                crate::extract::inline::markdown(notes, "https://github.com", &gh_inline_opts()).0;
            let trimmed: String = notes_md.chars().take(2_000).collect();
            md.push_str(trimmed.trim());
            md.push_str("\n\n");
        }
    }
    if n == 0 {
        return None;
    }
    Some(md)
}

// ── Commits ───────────────────────────────────────────────────

fn render_commits(doc: &Html) -> Option<String> {
    if let Some(md) = render_commits_modern(doc) {
        return Some(md);
    }
    render_commits_legacy(doc)
}

/// The 2025+ React list: rows carry stable data-testids and the
/// full sha in `data-commit-link`; the per-group h3 titles carry
/// the date (a row's own date renders client-side, as a skeleton
/// in the SSR HTML).
fn render_commits_modern(doc: &Html) -> Option<String> {
    let any_sel =
        Selector::parse("[data-testid='commit-group-title'], [data-testid='commit-row-item']")
            .ok()?;
    let msg_sel = Selector::parse("h4 a").ok()?;
    let author_sel = Selector::parse("a[href*='commits?author=']").ok()?;
    let mut md = String::from("# Commits\n\n");
    let mut n = 0;
    let mut date = String::new();
    for el in doc.select(&any_sel) {
        match el.value().attr("data-testid") {
            Some("commit-group-title") => {
                let t = text_of(el);
                date = iso_date(&t).unwrap_or(t);
            }
            Some("commit-row-item") => {
                let Some(msg_el) = el.select(&msg_sel).next() else {
                    continue;
                };
                let msg = text_of(msg_el);
                if msg.is_empty() {
                    continue;
                }
                let sha = el
                    .value()
                    .attr("data-commit-link")
                    .and_then(|href| href.rsplit('/').next())
                    .filter(|s| s.len() >= 7 && s.chars().all(|c| c.is_ascii_hexdigit()))
                    .map(|s| s.chars().take(7).collect::<String>())
                    .unwrap_or_default();
                let author = el
                    .select(&author_sel)
                    .next()
                    .map(text_of)
                    .filter(|a| !a.is_empty())
                    .unwrap_or_default();
                n += 1;
                md.push_str(&format!("- {msg}"));
                let meta: Vec<String> = [
                    (!author.is_empty()).then_some(format!("u/{author}")),
                    (!date.is_empty()).then_some(date.clone()),
                    (!sha.is_empty()).then_some(sha),
                ]
                .into_iter()
                .flatten()
                .collect();
                if !meta.is_empty() {
                    md.push_str(&format!(" ({})", meta.join(", ")));
                }
                md.push('\n');
                if n >= MAX_ITEMS {
                    break;
                }
            }
            _ => {}
        }
    }
    if n == 0 {
        return None;
    }
    Some(md)
}

/// "Commits on Sep 23, 2026" -> "2026-09-23"; `None` when the
/// shape is not recognized (the caller keeps the raw text).
fn iso_date(s: &str) -> Option<String> {
    let rest = s.trim().strip_prefix("Commits on")?.trim();
    let mut it = rest.split_whitespace();
    let mon = it.next()?;
    let day = it.next()?.trim_end_matches(',');
    let year = it.next()?;
    let m = match mon {
        "Jan" => 1,
        "Feb" => 2,
        "Mar" => 3,
        "Apr" => 4,
        "May" => 5,
        "Jun" => 6,
        "Jul" => 7,
        "Aug" => 8,
        "Sep" => 9,
        "Oct" => 10,
        "Nov" => 11,
        "Dec" => 12,
        _ => return None,
    };
    Some(format!("{year}-{m:02}-{day:0>2}"))
}

fn render_commits_legacy(doc: &Html) -> Option<String> {
    let commit_sel = Selector::parse("li.commit, div.commit").ok()?;
    let msg_sel = Selector::parse("a.js-navigation-open, a.Link--primary").ok()?;
    let author_sel = Selector::parse("a.commit-author, a.author").ok()?;
    let sha_sel = Selector::parse("code, a.sha").ok()?;
    let date_sel = Selector::parse("relative-time").ok()?;

    let mut md = String::from("# Commits\n\n");
    let mut n = 0;
    for c in doc.select(&commit_sel).take(MAX_ITEMS) {
        let Some(msg_el) = c.select(&msg_sel).next() else {
            continue;
        };
        let msg = text_of(msg_el);
        if msg.is_empty() {
            continue;
        }
        let author = c
            .select(&author_sel)
            .next()
            .map(text_of)
            .unwrap_or_default();
        let sha = c
            .select(&sha_sel)
            .next()
            .map(text_of)
            .filter(|s| s.len() >= 7 && s.chars().all(|ch| ch.is_ascii_hexdigit()))
            .map(|s| s.chars().take(7).collect::<String>())
            .unwrap_or_default();
        let date = c
            .select(&date_sel)
            .next()
            .and_then(|t| t.value().attr("datetime"))
            .map(|d| d.chars().take(10).collect::<String>())
            .unwrap_or_default();
        n += 1;
        md.push_str(&format!("- {msg}"));
        let meta: Vec<String> = [
            (!author.is_empty()).then_some(format!("u/{author}")),
            (!date.is_empty()).then_some(date),
            (!sha.is_empty()).then_some(sha),
        ]
        .into_iter()
        .flatten()
        .collect();
        if !meta.is_empty() {
            md.push_str(&format!(" ({})", meta.join(", ")));
        }
        md.push('\n');
    }
    if n == 0 {
        return None;
    }
    Some(md)
}

// Links ARE the content on GitHub threads (commit refs, cross
// links) : always render them.
fn gh_inline_opts() -> crate::extract::ExtractOptions {
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

    #[test]
    fn non_github_rejected() {
        assert!(extract("<html></html>", "https://example.com/x", &opts()).is_none());
        // repo front page: no adapter (generic pipeline handles it)
        assert!(
            extract(
                "<html></html>",
                "https://github.com/tokio-rs/tokio",
                &opts()
            )
            .is_none()
        );
    }

    #[test]
    fn issue_list_renders() {
        let html = r#"<html><body>
          <li class="js-issue-row">
            <svg class="octicon-issue-opened"></svg>
            <a class="js-navigation-open" href="/o/r/issues/42">Panic on empty input</a>
            <span class="Label--secondary">bug</span>
            <a aria-label="3 comments">3</a>
          </li>
          <li class="js-issue-row">
            <a class="js-navigation-open" href="/o/r/issues/41">Docs typo</a>
          </li>
        </body></html>"#;
        let ex = extract(html, "https://github.com/o/r/issues", &opts()).unwrap();
        assert_eq!(ex.via, Some("adapter:github-html"));
        assert!(ex.markdown.contains("Panic on empty input"));
        assert!(ex.markdown.contains("#42"));
        assert!(ex.markdown.contains("open"));
        assert!(ex.markdown.contains("[bug]"));
    }

    #[test]
    fn issue_thread_renders() {
        let html = r#"<html><body>
          <bdi class="js-issue-title">Panic on empty input</bdi>
          <svg class="octicon-issue-opened"></svg>
          <a class="author">alice</a><relative-time datetime="2026-01-02T10:00:00Z"></relative-time>
          <div class="timeline-comment">
            <a class="author">alice</a>
            <table><tr><td class="comment-body"><p>Steps to reproduce the <b>panic</b>:</p></td></tr></table>
          </div>
          <div class="timeline-comment">
            <a class="author">bob</a>
            <div class="comment-body"><p>Fixed in <a href="/o/r/pull/43">#43</a>.</p></div>
          </div>
        </body></html>"#;
        let ex = extract(html, "https://github.com/o/r/issues/42", &opts()).unwrap();
        assert!(ex.markdown.contains("# Panic on empty input"));
        assert!(ex.markdown.contains("o/r#42"));
        assert!(ex.markdown.contains("(opener)"));
        assert!(ex.markdown.contains("panic"));
    }

    #[test]
    fn releases_render() {
        let html = r#"<html><body>
          <section class="release">
            <h1><a href="/o/r/releases/tag/v2.0.0">v2.0.0</a></h1>
            <relative-time datetime="2026-02-01T00:00:00Z"></relative-time>
            <div class="markdown-body"><p>Big rewrite. <strong>Faster.</strong></p></div>
          </section>
        </body></html>"#;
        let ex = extract(html, "https://github.com/o/r/releases", &opts()).unwrap();
        assert!(ex.markdown.contains("## v2.0.0 : 2026-02-01"));
        assert!(ex.markdown.contains("Big rewrite"));
    }

    #[test]
    fn commits_render() {
        let html = r#"<html><body>
          <li class="commit">
            <a class="js-navigation-open" href="/o/r/commit/abc">fix: off-by-one</a>
            <a class="commit-author">carol</a>
            <code>abcdef1234567</code>
            <relative-time datetime="2026-03-01T00:00:00Z"></relative-time>
          </li>
        </body></html>"#;
        let ex = extract(html, "https://github.com/o/r/commits", &opts()).unwrap();
        assert!(ex.markdown.contains("fix: off-by-one"));
        assert!(ex.markdown.contains("u/carol"));
        assert!(ex.markdown.contains("abcdef1"));
    }

    #[test]
    fn releases_modern_render() {
        let html = r#"<html><body>
          <section id="release-v4.3.2" data-release-anchor="release-v4.3.2">
            <h2 class="sr-only" id="hd-1">v4.3.2</h2>
            <span class="tmp-mr-3 f1 text-bold d-inline"><a href="/o/r/releases/tag/v4.3.2" class="Link--primary Link">v4.3.2</a></span>
            <relative-time class="no-wrap" prefix="" datetime="2026-09-23T00:56:50Z">23 Sep 00:56</relative-time>
            <div data-test-selector="body-content" class="markdown-body tmp-my-3"><p>DonSeTch v4.3.2 notes</p></div>
          </section>
        </body></html>"#;
        let ex = extract(html, "https://github.com/o/r/releases", &opts()).unwrap();
        assert!(ex.markdown.contains("## v4.3.2 : 2026-09-23"));
        assert!(ex.markdown.contains("DonSeTch v4.3.2 notes"));
    }

    #[test]
    fn commits_modern_render_with_group_dates() {
        let html = r#"<html><body>
          <h3 data-testid="commit-group-title">Commits on Sep 23, 2026</h3>
          <li class="ListItem-module__listItem__wBJcm CommitRow-module__ListItem_0__u0LMo" data-testid="commit-row-item" data-commit-link="/o/r/commit/5d4d277ab42aff4d76d1ab3bcb05478ade43bceb">
            <div class="Title-module__container__ZzhV_"><h4 class="Title-module__heading__tHuYV"><a class="Title-module__anchor__dBbYy" href="/o/r/commit/5d4d277ab42aff4d76d1ab3bcb05478ade43bceb"><span>chore(release): 4.3.2</span></a></h4></div>
            <div class="CommitAttribution-module__CommitAttributionContainer__I_rfs"><a href="/dondai44423/donsetch/commits?author=dondai44423" aria-label="commits by dondai44423">dondai44423</a></div>
          </li>
          <h3 data-testid="commit-group-title">Commits on Sep 22, 2026</h3>
          <li class="ListItem-module__listItem__wBJcm CommitRow-module__ListItem_0__u0LMo" data-testid="commit-row-item" data-commit-link="/o/r/commit/2b50798aa11bb22cc33dd44ee55ff66aa77bb88c">
            <div class="Title-module__container__ZzhV_"><h4 class="Title-module__heading__tHuYV"><a href="/o/r/commit/2b50798aa11bb22cc33dd44ee55ff66aa77bb88c"><span>fix(fetch): reddit session</span></a></h4></div>
            <div class="CommitAttribution-module__CommitAttributionContainer__I_rfs"><a href="/o/r/commits?author=other" aria-label="commits by other">other</a></div>
          </li>
        </body></html>"#;
        let ex = extract(html, "https://github.com/o/r/commits/master", &opts()).unwrap();
        assert!(ex.markdown.contains("chore(release): 4.3.2"));
        assert!(ex.markdown.contains("(u/dondai44423, 2026-09-23, 5d4d277)"));
        assert!(ex.markdown.contains("(u/other, 2026-09-22, 2b50798)"));
    }
}
