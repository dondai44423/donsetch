//! Reddit cards: threads, listings, subreddit about pages and user
//! profiles as structured markdown from the site's own keyless JSON
//! endpoints, plus the small formatting helpers the SSR adapter
//! (`super::reddit_html`) renders through so both paths produce the
//! same card shapes.
//!
//! One plain-HTTP GET replaces the ghost-prone HTML scrape:
//! comment trees with scores/ages, listings with vote counts :
//! no JS shell, no login overlay. Anything unexpected returns
//! `None` → the caller falls back to the generic HTML path or
//! DonSift.

use serde_json::Value;

use crate::extract::{ContentKind, ExtractOptions, Extracted};

pub(crate) const MAX_COMMENTS: usize = 150;
pub(crate) const MAX_DEPTH: usize = 8;

/// Entry point. `url` is the final fetched URL (a `...json`
/// endpoint on any reddit host: the fetch-level rewrite keeps the
/// caller's host, issue #283).
pub fn extract(body: &[u8], url: &str, opts: &ExtractOptions) -> Option<Extracted> {
    let host = url::Url::parse(url).ok()?.host_str()?.to_string();
    if !crate::adapters::is_reddit_host(&host) {
        return None;
    }
    let v: Value = serde_json::from_slice(body).ok()?;

    let md = match &v {
        // Thread: [post-listing, comments-listing].
        Value::Array(arr) if arr.len() == 2 => {
            let post = arr
                .first()?
                .pointer("/data/children/0/data")
                .cloned()
                .unwrap_or_default();
            post.get("title")?;
            let comments = arr
                .get(1)?
                .pointer("/data/children")
                .cloned()
                .unwrap_or_default();
            render_thread(&post, &comments)
        }
        // Subreddit about (t5): /r/<sub>/about.json.
        Value::Object(_) if subreddit_data(&v).is_some() => {
            render_subreddit_card(subreddit_data(&v)?)
        }
        // User profile: /user/<name>/about.json.
        Value::Object(_) if user_data(&v).is_some() => render_user_card(user_data(&v)?),
        // /r/<sub>/about/rules.json : {"rules":[...]}.
        Value::Object(_) if v.get("rules").and_then(Value::as_array).is_some() => {
            let sub = path_sub(url).unwrap_or_else(|| "?".to_string());
            render_rules(&v, &sub)?
        }
        // Listing: {kind: Listing, data: {children: [t3...]}}.
        Value::Object(_) if v.pointer("/data/children/0/data/title").is_some() => {
            let children = v.pointer("/data/children")?.clone();
            render_listing(&children)
        }
        // User activity: children carry `body` (comments) or a mix.
        Value::Object(_) if v.pointer("/data/children/0/data/body").is_some() => {
            let children = v.pointer("/data/children")?.clone();
            render_mixed_listing(&children)
        }
        _ => return None,
    };

    let total = md.len();
    let max = opts.max_chars.unwrap_or(16_000).max(200);
    let (slice, next) = crate::extract::paginate_public(&md, opts.offset, max);
    let (kind, blocks) = if v.is_array() {
        (ContentKind::Forum, count_comments(&v))
    } else if v.get("rules").is_some() {
        (
            ContentKind::Listing,
            v.get("rules").and_then(Value::as_array).map_or(0, Vec::len),
        )
    } else if v.pointer("/data/children").is_some() {
        (ContentKind::Listing, children_count(&v))
    } else {
        (ContentKind::Article, 1)
    };
    Some(Extracted {
        tokens_est: slice.len() / 4,
        markdown: slice,
        title: title_of(&v, url),
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
        quality: 0.95,
        pdf_pages: None,
        images: Vec::new(),
        fingerprint: None,
        via: Some("adapter:reddit-json"),
        partial: None,
    })
}

// ── shared card helpers (the SSR adapter renders through these) ──

/// The head of a thread card: `# {title}` plus the byline line.
/// `sub` is the bare subreddit name.
pub(crate) fn thread_head(
    title: &str,
    author: &str,
    score: i64,
    created_utc: Option<f64>,
    comments_n: i64,
    sub: &str,
    flags: &[&str],
) -> String {
    let age = age_of(created_utc);
    let flag_str = if flags.is_empty() {
        String::new()
    } else {
        format!(" · [{}]", flags.join(" "))
    };
    format!(
        "# {title}\nu/{author} · {score} pts · {age} · {comments_n} comments · r/{sub}{flag_str}\n\n"
    )
}

/// One comment's byline line (body appended by the caller).
pub(crate) fn comment_head(
    indent: &str,
    author: &str,
    score: i64,
    created_utc: Option<f64>,
    tags: &str,
) -> String {
    let age = age_of(created_utc);
    format!("{indent}**u/{author}** · {score} pts · {age}{tags}\n")
}

/// One numbered listing line.
#[allow(clippy::too_many_arguments)]
pub(crate) fn listing_line(
    n: usize,
    prefix: &str,
    title: &str,
    domain: &str,
    score: i64,
    author: &str,
    created_utc: Option<f64>,
    comments: i64,
    flair: &str,
) -> String {
    let age = age_of(created_utc);
    format!(
        "{n}. {prefix}**{title}** ({domain}) · {score} pts · u/{author} · {age} · {comments} comments{flair}\n\n"
    )
}

// ── Thread ────────────────────────────────────────────────────

fn render_thread(post: &Value, comments: &Value) -> String {
    let title = post.get("title").and_then(Value::as_str).unwrap_or("");
    let sub = post.get("subreddit").and_then(Value::as_str).unwrap_or("?");
    let author = post
        .get("author")
        .and_then(Value::as_str)
        .unwrap_or("[deleted]");
    let score = post.get("score").and_then(Value::as_i64).unwrap_or(0);
    let age_created = post.get("created_utc").and_then(Value::as_f64);
    let comments_n = post
        .get("num_comments")
        .and_then(Value::as_i64)
        .unwrap_or(0);

    let flags: Vec<&str> = [
        post.get("over_18")
            .and_then(Value::as_bool)
            .unwrap_or(false)
            .then_some("NSFW"),
        post.get("spoiler")
            .and_then(Value::as_bool)
            .unwrap_or(false)
            .then_some("spoiler"),
        post.get("locked")
            .and_then(Value::as_bool)
            .unwrap_or(false)
            .then_some("locked"),
        post.get("stickied")
            .and_then(Value::as_bool)
            .unwrap_or(false)
            .then_some("sticky"),
        post.get("pinned")
            .and_then(Value::as_bool)
            .unwrap_or(false)
            .then_some("pinned"),
    ]
    .into_iter()
    .flatten()
    .collect();

    let mut md = thread_head(title, author, score, age_created, comments_n, sub, &flags);

    if let Some(flair) = post.get("link_flair_text").and_then(Value::as_str)
        && !flair.is_empty()
    {
        md.push_str(&format!("*flair: {flair}*\n\n"));
    }

    // Link posts: the destination URL. Self posts: the body.
    let selftext = post.get("selftext").and_then(Value::as_str).unwrap_or("");
    if !selftext.is_empty() && selftext != "[removed]" && selftext != "[deleted]" {
        md.push_str(selftext.trim());
        md.push_str("\n\n---\n\n");
    } else if let Some(dest) = post.get("url").and_then(Value::as_str)
        && !dest.contains("reddit.com")
    {
        md.push_str(&format!("→ {dest}\n\n---\n\n"));
    }

    let mut rendered = 0usize;
    if let Some(children) = comments.as_array() {
        for child in children {
            render_comment(child, 0, &mut md, &mut rendered);
        }
    }
    if rendered >= MAX_COMMENTS {
        md.push_str(&format!(
            "*(showing {MAX_COMMENTS} of {comments_n} comments)*\n"
        ));
    }
    md
}

fn render_comment(node: &Value, depth: usize, md: &mut String, rendered: &mut usize) {
    if *rendered >= MAX_COMMENTS {
        return;
    }
    match node.get("kind").and_then(Value::as_str) {
        Some("t1") => {}
        Some("more") => {
            let n = node
                .pointer("/data/count")
                .and_then(Value::as_i64)
                .unwrap_or(0);
            if n > 0 {
                let indent = "  ".repeat(depth.min(MAX_DEPTH));
                md.push_str(&format!(
                    "{indent}*(+{n} more replies : deeper thread)*\n\n"
                ));
            }
            return;
        }
        _ => return,
    }
    let d = node.get("data").cloned().unwrap_or_default();
    let author = d
        .get("author")
        .and_then(Value::as_str)
        .unwrap_or("[deleted]");
    let score = d.get("score").and_then(Value::as_i64).unwrap_or(0);
    let body = d.get("body").and_then(Value::as_str).unwrap_or("");
    let is_op = d
        .get("is_submitter")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let stickied = d.get("stickied").and_then(Value::as_bool).unwrap_or(false);
    let controversial = d
        .get("controversiality")
        .and_then(Value::as_i64)
        .unwrap_or(0)
        > 0;

    let indent = "  ".repeat(depth.min(MAX_DEPTH));
    let mut tags = String::new();
    if is_op {
        tags.push_str(" · OP");
    }
    if stickied {
        tags.push_str(" · sticky");
    }
    if controversial {
        tags.push_str(" · contested");
    }
    md.push_str(&comment_head(
        &indent,
        author,
        score,
        d.get("created_utc").and_then(Value::as_f64),
        &tags,
    ));

    if body.is_empty() || body == "[removed]" || body == "[deleted]" {
        md.push_str(&format!("{indent}[removed]\n\n"));
    } else {
        for line in body.lines() {
            md.push_str(&format!("{indent}{line}\n"));
        }
        md.push('\n');
    }
    *rendered += 1;

    // replies: "" (string) when there are none.
    if let Some(children) = d
        .pointer("/replies/data/children")
        .and_then(Value::as_array)
    {
        for child in children {
            render_comment(child, depth + 1, md, rendered);
        }
    }
}

// ── Listing ───────────────────────────────────────────────────

/// One t3 child as a numbered line (None when it carries no title).
fn listing_item(n: usize, d: &Value) -> Option<String> {
    let title = d.get("title").and_then(Value::as_str).unwrap_or("");
    if title.is_empty() {
        return None;
    }
    let domain = d.get("domain").and_then(Value::as_str).unwrap_or("?");
    let score = d.get("score").and_then(Value::as_i64).unwrap_or(0);
    let author = d
        .get("author")
        .and_then(Value::as_str)
        .unwrap_or("[deleted]");
    let comments = d.get("num_comments").and_then(Value::as_i64).unwrap_or(0);
    let nsfw = d.get("over_18").and_then(Value::as_bool).unwrap_or(false);
    let sticky = d.get("stickied").and_then(Value::as_bool).unwrap_or(false);
    let prefix = if sticky {
        "[sticky] "
    } else if nsfw {
        "[NSFW] "
    } else {
        ""
    };
    let flair = d
        .get("link_flair_text")
        .and_then(Value::as_str)
        .filter(|f| !f.is_empty())
        .map(|f| format!(" · {f}"))
        .unwrap_or_default();
    Some(listing_line(
        n,
        prefix,
        title,
        domain,
        score,
        author,
        d.get("created_utc").and_then(Value::as_f64),
        comments,
        &flair,
    ))
}

fn render_listing(children: &Value) -> String {
    let mut md = String::new();
    let mut n = 0usize;
    if let Some(posts) = children.as_array() {
        for post in posts {
            let d = match post.get("data") {
                Some(d) => d,
                None => continue,
            };
            if d.get("stickied").and_then(Value::as_bool).unwrap_or(false) && n > 0 {
                continue; // one sticky at top is enough context
            }
            let Some(line) = listing_item(n + 1, d) else {
                continue;
            };
            n += 1;
            md.push_str(&line);
            // Sticky announcements often carry the rules : first
            // 200 chars of the self-text.
            if d.get("stickied").and_then(Value::as_bool).unwrap_or(false)
                && let Some(st) = d.get("selftext").and_then(Value::as_str)
                && !st.is_empty()
            {
                let preview: String = st.chars().take(200).collect();
                md.push_str(&format!("   > {preview}\n\n"));
            }
        }
    }
    md
}

/// A user's activity: t3 posts render as listing lines, t1 comments
/// as compact excerpts (the JSON `comments`/overview listings).
fn render_mixed_listing(children: &Value) -> String {
    let mut md = String::new();
    let mut n = 0usize;
    for child in children.as_array().into_iter().flatten() {
        let Some(d) = child.get("data") else {
            continue;
        };
        let line = match child.get("kind").and_then(Value::as_str) {
            Some("t1") => {
                let body = d.get("body").and_then(Value::as_str).unwrap_or("");
                let collapsed: String = body.split_whitespace().collect::<Vec<_>>().join(" ");
                let excerpt: String = collapsed.chars().take(200).collect();
                if excerpt.is_empty() {
                    continue;
                }
                let author = d
                    .get("author")
                    .and_then(Value::as_str)
                    .unwrap_or("[deleted]");
                let score = d.get("score").and_then(Value::as_i64).unwrap_or(0);
                let sub = d.get("subreddit").and_then(Value::as_str).unwrap_or("?");
                let age = age_of(d.get("created_utc").and_then(Value::as_f64));
                n += 1;
                format!("{n}. u/{author} · {score} pts · {age} · r/{sub} · comment: {excerpt}\n\n")
            }
            _ => match listing_item(n + 1, d) {
                Some(l) => {
                    n += 1;
                    l
                }
                None => continue,
            },
        };
        md.push_str(&line);
    }
    md
}

// ── Subreddit about / user profile / rules ────────────────────

/// /about.json (t5) anchor fields: required so an unrelated object
/// never claims the shape.
fn subreddit_data(v: &Value) -> Option<&Value> {
    let d = v.pointer("/data")?;
    d.get("display_name")?.as_str()?;
    d.get("subscribers")?.as_u64()?;
    d.get("created_utc")?.as_f64()?;
    Some(d)
}

/// A user profile (t2): karma is the anchor no other shape carries.
fn user_data(v: &Value) -> Option<&Value> {
    let d = v.pointer("/data")?;
    let name = d.get("name")?.as_str()?;
    if name.starts_with("t1_") || name.starts_with("t3_") {
        return None;
    }
    d.get("link_karma")?;
    d.get("created_utc")?.as_f64()?;
    Some(d)
}

fn render_subreddit_card(d: &Value) -> String {
    let name = d.get("display_name").and_then(Value::as_str).unwrap_or("?");
    let mut md = format!("# r/{name}\n");
    if let Some(t) = d.get("title").and_then(Value::as_str)
        && !t.is_empty()
    {
        md.push_str(&format!("{t}\n"));
    }
    let mut facts: Vec<String> = Vec::new();
    if let Some(n) = d.get("subscribers").and_then(Value::as_u64) {
        facts.push(format!("{} members", crate::adapters::human_count(n)));
    }
    if let Some(n) = d.get("active_user_count").and_then(Value::as_u64) {
        facts.push(format!("{} online", crate::adapters::human_count(n)));
    }
    if let Some(ts) = d.get("created_utc").and_then(Value::as_f64) {
        facts.push(format!("created {}", date_from_epoch(ts)));
    }
    if let Some(t) = d.get("subreddit_type").and_then(Value::as_str)
        && !t.is_empty()
    {
        facts.push(t.to_string());
    }
    let mut flags: Vec<&str> = Vec::new();
    if d.get("over18").and_then(Value::as_bool).unwrap_or(false) {
        flags.push("NSFW");
    }
    if d.get("quarantine")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        flags.push("quarantined");
    }
    if !flags.is_empty() {
        facts.push(flags.join(" "));
    }
    if !facts.is_empty() {
        md.push_str(&format!("{}\n", facts.join(" · ")));
    }
    if let Some(desc) = d.get("public_description").and_then(Value::as_str) {
        let desc = desc.trim();
        if !desc.is_empty() {
            let desc: String = desc.chars().take(500).collect();
            md.push_str(&format!("\n{desc}\n"));
        }
    }
    md
}

fn render_user_card(d: &Value) -> String {
    let name = d.get("name").and_then(Value::as_str).unwrap_or("?");
    let mut md = format!("# u/{name}\n");
    let mut facts: Vec<String> = Vec::new();
    let link = d
        .get("link_karma")
        .and_then(Value::as_i64)
        .unwrap_or(0)
        .max(0);
    let comment = d
        .get("comment_karma")
        .and_then(Value::as_i64)
        .unwrap_or(0)
        .max(0);
    let total = d
        .get("total_karma")
        .and_then(Value::as_i64)
        .map(|t| t.max(0))
        .unwrap_or(link + comment);
    facts.push(format!(
        "{} karma ({} link · {} comment)",
        crate::adapters::human_count(total as u64),
        crate::adapters::human_count(link as u64),
        crate::adapters::human_count(comment as u64)
    ));
    if let Some(ts) = d.get("created_utc").and_then(Value::as_f64) {
        facts.push(format!("created {}", date_from_epoch(ts)));
    }
    let mut flags: Vec<&str> = Vec::new();
    if d.get("is_employee")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        flags.push("reddit employee");
    }
    if d.get("is_mod").and_then(Value::as_bool).unwrap_or(false) {
        flags.push("mod");
    }
    if d.get("verified").and_then(Value::as_bool).unwrap_or(false) {
        flags.push("verified");
    }
    if !flags.is_empty() {
        facts.push(flags.join(" "));
    }
    md.push_str(&format!("{}\n", facts.join(" · ")));
    md
}

fn render_rules(v: &Value, sub: &str) -> Option<String> {
    let rules = v.get("rules")?.as_array()?;
    let mut md = format!("# r/{sub} rules\n\n");
    let mut n = 0usize;
    for r in rules {
        let title = r.get("short_name").and_then(Value::as_str).unwrap_or("");
        if title.is_empty() {
            continue;
        }
        n += 1;
        md.push_str(&format!("{n}. {title}"));
        if let Some(d) = r.get("description").and_then(Value::as_str) {
            let d: String = d.split_whitespace().collect::<Vec<_>>().join(" ");
            let d: String = d.chars().take(240).collect();
            if !d.is_empty() {
                md.push_str(&format!(" : {d}"));
            }
        }
        md.push('\n');
    }
    if n == 0 {
        return None;
    }
    Some(md)
}

// ── helpers ───────────────────────────────────────────────────

fn title_of(v: &Value, url: &str) -> Option<String> {
    if v.is_array() {
        v.pointer("/0/data/children/0/data/title")
            .and_then(Value::as_str)
            .map(String::from)
    } else if let Some(d) = subreddit_data(v) {
        d.get("display_name")
            .and_then(Value::as_str)
            .map(|s| format!("r/{s}"))
    } else if let Some(d) = user_data(v) {
        d.get("name")
            .and_then(Value::as_str)
            .map(|n| format!("u/{n}"))
    } else if v.get("rules").is_some() {
        path_sub(url).map(|s| format!("r/{s} rules"))
    } else if let Some(u) = path_user(url) {
        // A user's submitted/comments listing keys on the user.
        Some(format!("u/{u}"))
    } else {
        v.pointer("/data/children/0/data/subreddit")
            .and_then(Value::as_str)
            .map(|s| format!("r/{s}"))
    }
}

fn path_segs(url: &str) -> Vec<String> {
    url::Url::parse(url)
        .ok()
        .map(|u| {
            u.path()
                .split('/')
                .filter(|s| !s.is_empty())
                .map(String::from)
                .collect()
        })
        .unwrap_or_default()
}

fn path_sub(url: &str) -> Option<String> {
    let segs = path_segs(url);
    (segs.len() >= 2 && segs[0] == "r").then(|| segs[1].clone())
}

fn path_user(url: &str) -> Option<String> {
    let segs = path_segs(url);
    (segs.len() >= 2 && matches!(segs[0].as_str(), "user" | "u")).then(|| segs[1].clone())
}

fn children_count(v: &Value) -> usize {
    v.pointer("/data/children")
        .and_then(Value::as_array)
        .map_or(0, std::vec::Vec::len)
}

fn count_comments(v: &Value) -> usize {
    v.get(1)
        .and_then(|l| l.pointer("/data/children"))
        .and_then(Value::as_array)
        .map_or(0, |arr| {
            arr.iter()
                .filter(|c| c.get("kind").and_then(Value::as_str) == Some("t1"))
                .count()
        })
}

/// created_utc → compact relative age ("now", "5m", "3h", "2d",
/// "1w", "3mo", "2y").
pub(crate) fn age_of(created: Option<f64>) -> String {
    let Some(t) = created else {
        return "?".to_string();
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0);
    let secs = (now - t).max(0.0);
    match secs as u64 {
        0..=59 => "now".to_string(),
        60..=3599 => format!("{}m", secs as u64 / 60),
        3600..=86_399 => format!("{}h", secs as u64 / 3600),
        86_400..=604_799 => format!("{}d", secs as u64 / 86_400),
        604_800..=2_591_999 => format!("{}w", secs as u64 / 604_800),
        2_592_000..=31_535_999 => format!("{}mo", secs as u64 / 2_592_000),
        _ => format!("{}y", secs as u64 / 31_536_000),
    }
}

/// Epoch seconds → "YYYY-MM-DD" (UTC).
pub(crate) fn date_from_epoch(secs: f64) -> String {
    let (y, m, d) = civil_from_days((secs as i64).div_euclid(86_400));
    format!("{y:04}-{m:02}-{d:02}")
}

/// "2026-09-22T19:34:03.310000+0000" → epoch seconds (UTC).
/// Reddit SSR timestamps are `+0000`; anything without a full
/// `YYYY-MM-DDTHH:MM:SS` head is rejected.
pub(crate) fn epoch_from_iso(ts: &str) -> Option<f64> {
    let n = |a: usize, z: usize| -> Option<i64> { ts.get(a..z)?.parse().ok() };
    let (y, mo, d) = (n(0, 4)?, n(5, 7)?, n(8, 10)?);
    let (h, mi, s) = (n(11, 13)?, n(14, 16)?, n(17, 19)?);
    if ts.len() < 19 || !(1..=12).contains(&mo) || !(1..=31).contains(&d) {
        return None;
    }
    let days = days_from_civil(y, mo as u32, d as u32);
    Some((days * 86_400 + h * 3600 + mi * 60 + s) as f64)
}

/// Howard Hinnant's days-from-civil (proleptic Gregorian, UTC).
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as u64;
    let mp = if m > 2 { m - 3 } else { m + 9 } as u64;
    let doy = (153 * mp + 2) / 5 + d as u64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe as i64 - 719_468
}

/// The inverse: epoch days → (year, month, day).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (y + i64::from(m <= 2), m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts() -> ExtractOptions {
        ExtractOptions::default()
    }

    const THREAD: &str = r#"[
      {"kind":"Listing","data":{"children":[{"kind":"t3","data":{
        "title":"Why Rust is great","subreddit":"rust","author":"alice",
        "score":421,"created_utc":1755800000.0,"num_comments":42,
        "selftext":"Body text here","stickied":false,"over_18":false,
        "url":"https://example.com/self"}}]}},
      {"kind":"Listing","data":{"children":[
        {"kind":"t1","data":{"author":"bob","score":88,"created_utc":1755800100.0,
          "body":"First!","is_submitter":false,
          "replies":{"data":{"children":[
            {"kind":"t1","data":{"author":"alice","score":30,"created_utc":1755800200.0,
              "body":"Thanks","is_submitter":true,"replies":""}},
            {"kind":"more","data":{"count":7,"children":["x"]}}]}}}},
        {"kind":"t1","data":{"author":"carol","score":-2,"created_utc":1755800300.0,
          "body":"[removed]","replies":""}}
      ]}}
    ]"#;

    #[test]
    fn thread_renders() {
        let ex = extract(
            THREAD.as_bytes(),
            "https://old.reddit.com/r/rust/comments/abc/x.json",
            &opts(),
        )
        .expect("thread");
        assert_eq!(ex.via, Some("adapter:reddit-json"));
        assert!(ex.markdown.contains("# Why Rust is great"));
        assert!(ex.markdown.contains("u/alice"));
        assert!(ex.markdown.contains("Body text here"));
        assert!(ex.markdown.contains("First!"));
        // Nested reply indented, OP-tagged.
        assert!(ex.markdown.contains("u/alice** · 30 pts"));
        assert!(ex.markdown.contains("· OP"));
        // Removed comment.
        assert!(ex.markdown.contains("[removed]"));
        // "more" node → note, not a panic.
        assert!(ex.markdown.contains("+7 more replies"));
        assert_eq!(ex.content_kind, ContentKind::Forum);
    }

    #[test]
    fn listing_renders() {
        let listing = r#"{"kind":"Listing","data":{"children":[
          {"kind":"t3","data":{"title":"Sticky: rules","stickied":true,"domain":"self.rust","subreddit":"rust",
            "score":1,"author":"mods","num_comments":2,"created_utc":1755800000.0,
            "selftext":"Be nice. Read the FAQ first.","over_18":false}},
          {"kind":"t3","data":{"title":"Real post","domain":"example.com","score":99,
            "author":"dave","num_comments":5,"created_utc":1755800500.0,
            "over_18":false,"stickied":false,"selftext":""}}
        ]}}"#;
        let ex = extract(
            listing.as_bytes(),
            "https://old.reddit.com/r/rust.json",
            &opts(),
        )
        .expect("listing");
        assert!(ex.markdown.contains("Sticky: rules"));
        assert!(ex.markdown.contains("Be nice."));
        assert!(ex.markdown.contains("Real post"));
        assert!(ex.markdown.contains("99 pts"));
        assert_eq!(ex.content_kind, ContentKind::Listing);
        assert_eq!(ex.title.as_deref(), Some("r/rust"));
    }

    #[test]
    fn subreddit_about_card() {
        let about = r#"{"kind":"t5","data":{"display_name":"rust",
          "title":"The Rust Programming Language","subscribers":350000,
          "active_user_count":1400,"created_utc":1234567890.0,
          "subreddit_type":"public","over18":false,
          "public_description":"A place for all things Rust."}}"#;
        let ex = extract(
            about.as_bytes(),
            "https://www.reddit.com/r/rust/about.json",
            &opts(),
        )
        .expect("about");
        assert_eq!(ex.title.as_deref(), Some("r/rust"));
        assert!(ex.markdown.contains("# r/rust"));
        assert!(ex.markdown.contains("350.0k members"));
        assert!(ex.markdown.contains("created 2009-02-13"));
        assert!(ex.markdown.contains("A place for all things Rust."));
        assert_eq!(ex.content_kind, ContentKind::Article);
    }

    #[test]
    fn user_profile_card() {
        let u = r#"{"kind":"t2","data":{"name":"spez","link_karma":100,
          "comment_karma":200,"total_karma":300,"created_utc":1111111111.0,
          "is_employee":true}}"#;
        let ex = extract(
            u.as_bytes(),
            "https://www.reddit.com/user/spez/about.json",
            &opts(),
        )
        .expect("user");
        assert_eq!(ex.title.as_deref(), Some("u/spez"));
        assert!(ex.markdown.contains("# u/spez"));
        assert!(ex.markdown.contains("300 karma (100 link · 200 comment)"));
        assert!(ex.markdown.contains("reddit employee"));
    }

    #[test]
    fn rules_card() {
        let r = r#"{"rules":[
          {"short_name":"Be civil","description":"Treat others   with respect."},
          {"short_name":"No spam","description":"Self-promotion limits apply."}]}"#;
        let ex = extract(
            r.as_bytes(),
            "https://www.reddit.com/r/rust/about/rules.json",
            &opts(),
        )
        .expect("rules");
        assert_eq!(ex.title.as_deref(), Some("r/rust rules"));
        assert!(
            ex.markdown
                .contains("1. Be civil : Treat others with respect.")
        );
        assert!(ex.markdown.contains("2. No spam"));
    }

    #[test]
    fn user_activity_listing_renders_comments() {
        let m = r#"{"kind":"Listing","data":{"children":[
          {"kind":"t1","data":{"author":"alice","score":12,"subreddit":"rust",
            "created_utc":1755800000.0,"body":"I think  the borrow checker is great."}},
          {"kind":"t3","data":{"title":"A post","domain":"example.com","score":3,
            "author":"alice","num_comments":0,"created_utc":1755800100.0,
            "over_18":false,"stickied":false}}
        ]}}"#;
        let ex = extract(
            m.as_bytes(),
            "https://www.reddit.com/user/alice/comments.json",
            &opts(),
        )
        .expect("mixed");
        assert_eq!(ex.title.as_deref(), Some("u/alice"));
        assert!(ex.markdown.contains("1. u/alice · 12 pts"));
        assert!(
            ex.markdown
                .contains("r/rust · comment: I think the borrow checker is great."),
            "{}",
            ex.markdown
        );
        assert!(ex.markdown.contains("2. **A post**"));
    }

    #[test]
    fn epoch_and_date_math() {
        // 1_700_000_000 = 2023-11-14 22:13:20 UTC.
        assert_eq!(date_from_epoch(1_700_000_000.0), "2023-11-14");
        assert_eq!(date_from_epoch(0.0), "1970-01-01");
        assert_eq!(
            epoch_from_iso("2023-11-14T22:13:20+0000"),
            Some(1_700_000_000.0)
        );
        assert_eq!(
            epoch_from_iso("2026-09-22T19:34:03.310000+0000"),
            Some(1_790_105_643.0)
        );
        assert_eq!(epoch_from_iso("not a date"), None);
        assert_eq!(epoch_from_iso("2026-13-40T00:00:00"), None);
        // Round trip through the civil-date math.
        let ts = epoch_from_iso("2026-09-22T19:34:03.310000+0000").unwrap();
        assert_eq!(date_from_epoch(ts), "2026-09-22");
    }

    #[test]
    fn wrong_site_json_rejected() {
        assert!(
            extract(
                THREAD.as_bytes(),
                "https://registry.npmjs.org/react",
                &opts()
            )
            .is_none()
        );
        // Look-alike domains are not reddit (a bare suffix check
        // used to claim them).
        assert!(
            extract(
                THREAD.as_bytes(),
                "https://notreddit.com/r/rust/comments/abc/x.json",
                &opts()
            )
            .is_none()
        );
    }

    #[test]
    fn garbage_rejected() {
        assert!(extract(b"not json", "https://old.reddit.com/r/rust.json", &opts()).is_none());
        assert!(extract(b"[]", "https://old.reddit.com/r/rust.json", &opts()).is_none());
    }
}
