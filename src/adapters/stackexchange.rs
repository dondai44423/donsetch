//! Stack Exchange adapter: question + answers as a structured QA
//! tree with accepted-answer marking, scores, and authorship.
//! Stack Exchange HTML is server-rendered : this restructures
//! what the generic pipeline flattens (vote columns, sidebars).

use scraper::{ElementRef, Html, Selector};

use crate::extract::{ContentKind, ExtractOptions, Extracted, inline};

/// Stack Exchange sites (the DOM shape is shared platform-wide):
/// label-suffix matched, so meta.* subdomains and the network's
/// stand-alone domains are covered too.
const HOST_SUFFIXES: [&str; 6] = [
    "stackoverflow.com",
    "stackexchange.com",
    "superuser.com",
    "serverfault.com",
    "askubuntu.com",
    "mathoverflow.net",
];

/// `stackoverflow.com`, `meta.stackoverflow.com`, ... but never a
/// look-alike like `notstackoverflow.com`.
fn is_se_host(host: &str) -> bool {
    HOST_SUFFIXES
        .iter()
        .any(|s| host == *s || host.strip_suffix(s).is_some_and(|p| p.ends_with('.')))
}

const MAX_ANSWERS: usize = 10;

pub fn extract(html: &str, url: &str, opts: &ExtractOptions) -> Option<Extracted> {
    if opts.selector.is_some() {
        return None;
    }
    let u = url::Url::parse(url).ok()?;
    let host = u.host_str()?;
    if !is_se_host(host) {
        return None;
    }
    // Question pages only: /questions/<digits>/... or /q/<digits>.
    // (/questions/tagged/x and /questions/ask are lists/forms.)
    let path = u.path();
    let id = path
        .strip_prefix("/questions/")
        .or_else(|| path.strip_prefix("/q/"))
        .and_then(|rest| rest.split('/').next())
        .unwrap_or("");
    if id.is_empty() || !id.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }

    let doc = Html::parse_document(html);
    let q_sel = Selector::parse("div.question, #question").ok()?;
    let question = doc.select(&q_sel).next()?;

    let title = doc
        .select(&Selector::parse("a.question-hyperlink").ok()?)
        .next()
        .map(text_of)
        .filter(|t| !t.is_empty())?;

    let mut md = format!("# {title}\n");
    if let Some((score, body, author, date)) = post_parts(&question) {
        let who = if author.is_empty() {
            "anon".to_string()
        } else {
            format!("u/{author}")
        };
        md.push_str(&format!("**Q · {score} pts · {who} · {date}**\n\n"));
        md.push_str(body.trim());
        md.push_str("\n\n");
    }

    let ans_sel = Selector::parse("div.answer").ok()?;
    let mut answers = doc.select(&ans_sel).collect::<Vec<_>>();
    // Highest score first (SE renders them sorted already, but be
    // honest about it rather than trusting DOM order).
    answers.sort_by_key(|a| -score_of(a));
    let mut n = 0;
    for ans in answers.iter().take(MAX_ANSWERS) {
        let Some((score, body, author, date)) = post_parts(ans) else {
            continue;
        };
        let accepted = ans.value().classes().any(|c| c == "accepted-answer");
        n += 1;
        let mark = if accepted { " ✓ ACCEPTED" } else { "" };
        let who = if author.is_empty() {
            "anon".to_string()
        } else {
            format!("u/{author}")
        };
        md.push_str(&format!(
            "---\n\n**A{n} · {score} pts{mark} · {who} · {date}**\n\n"
        ));
        md.push_str(body.trim());
        md.push_str("\n\n");
    }
    if n == 0 {
        // A question page with zero answers is still worth the
        // adapter treatment (question body survived above).
        md.push_str("*(no answers yet)*\n");
    }

    let total = md.len();
    let max = opts.max_chars.unwrap_or(16_000).max(200);
    let (slice, next) = crate::extract::paginate_public(&md, opts.offset, max);
    Some(Extracted {
        tokens_est: slice.len() / 4,
        markdown: slice,
        title: Some(title),
        byline: None,
        published: None,
        site: Some(
            host.split('.')
                .next()
                .unwrap_or("stackexchange")
                .to_string(),
        ),
        total_chars: total,
        next_offset: next,
        blocks_total: n,
        blocks_shown: n,
        thin: false,
        content_kind: ContentKind::Forum,
        lang: "en".to_string(),
        quality: 0.9,
        pdf_pages: None,
        images: Vec::new(),
        fingerprint: None,
        via: Some("adapter:stackexchange"),
        partial: None,
    })
}

// Immutable public filter: base=withbody, plus question.answers,
// question.comments, answer.body, answer.comments and comment.body.
const API_FILTER: &str = "!)cN)B)B5rJi5kvLl5pKw5R)I8TDRK022n8V)6wtAusFra";

static API_BACKOFF_UNTIL: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

pub fn record_api_backoff(body: &[u8]) {
    if let Ok(v) = serde_json::from_slice::<serde_json::Value>(body)
        && let Some(seconds) = v["backoff"].as_u64()
    {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        API_BACKOFF_UNTIL.fetch_max(
            now.saturating_add(seconds),
            std::sync::atomic::Ordering::Relaxed,
        );
    }
}

pub fn api_url(u: &url::Url) -> Option<String> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    if now < API_BACKOFF_UNTIL.load(std::sync::atomic::Ordering::Relaxed) {
        return None;
    }
    if !matches!(u.host_str()?, "stackoverflow.com" | "www.stackoverflow.com") {
        return None;
    }
    let id = u
        .path()
        .strip_prefix("/questions/")
        .or_else(|| u.path().strip_prefix("/q/"))?
        .split('/')
        .next()?;
    if id.is_empty() || id.len() > 20 || !id.bytes().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let mut api =
        url::Url::parse(&format!("https://api.stackexchange.com/2.3/questions/{id}")).ok()?;
    api.query_pairs_mut()
        .append_pair("site", "stackoverflow")
        .append_pair("filter", API_FILTER);
    Some(api.to_string())
}

fn api_question(body: &[u8], url: &str) -> Option<serde_json::Value> {
    let u = url::Url::parse(url).ok()?;
    if u.host_str()? != "api.stackexchange.com"
        || !u
            .query_pairs()
            .any(|(k, v)| k == "site" && v == "stackoverflow")
    {
        return None;
    }
    let id: u64 = u.path().strip_prefix("/2.3/questions/")?.parse().ok()?;
    let v: serde_json::Value = serde_json::from_slice(body).ok()?;
    if v.get("error_id").is_some() {
        return None;
    }
    let items = v.get("items")?.as_array()?;
    if items.len() != 1 {
        return None;
    }
    let q = items[0].clone();
    if q["question_id"].as_u64()? != id || q["body"].as_str()?.trim().is_empty() {
        return None;
    }
    let source = url::Url::parse(q["link"].as_str()?).ok()?;
    if source.scheme() != "https"
        || !matches!(
            source.host_str()?,
            "stackoverflow.com" | "www.stackoverflow.com"
        )
        || !source.username().is_empty()
        || source.password().is_some()
        || !source.path().starts_with("/questions/")
    {
        return None;
    }
    if source.path().split('/').nth(2)?.parse::<u64>().ok()? != id {
        return None;
    }
    q["title"].as_str()?;
    if q["answers"].as_array().is_none() && q["answer_count"].as_u64() != Some(0) {
        return None;
    }
    Some(q)
}

pub fn api_payload_valid(body: &[u8], url: &str) -> bool {
    api_question(body, url).is_some()
}

pub fn extract_api(body: &[u8], url: &str, opts: &ExtractOptions) -> Option<Extracted> {
    let q = api_question(body, url)?;
    let source = q["link"].as_str()?;
    let title = api_text(q["title"].as_str()?);
    let mut html = format!(
        "<html><head><title>{title}</title></head><body><article><h1>{title}</h1><h2>Question</h2>"
    );
    api_post(&q, &mut html);
    let answers = q["answers"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default();
    let mut found = 0;
    for answer in answers.iter().take(100) {
        if answer["question_id"].as_u64() != q["question_id"].as_u64()
            || answer["body"].as_str().is_none()
        {
            continue;
        }
        found += 1;
        let accepted = if answer["is_accepted"] == true {
            " · ACCEPTED"
        } else {
            ""
        };
        html.push_str(&format!("<h2>Answer {found}{accepted}</h2>"));
        api_post(answer, &mut html);
    }
    html.push_str("</article></body></html>");
    let mut ex = crate::extract::extract(html.as_bytes(), "text/html", source, opts).ok()?;
    ex.via = Some("adapter:stackexchange-api");
    ex.content_kind = ContentKind::Forum;
    let total = q["answer_count"]
        .as_u64()
        .and_then(|n| usize::try_from(n).ok());
    if total.is_none_or(|n| found < n) {
        ex.partial = Some(crate::extract::PartialContent {
            reason: "public API returned a subset of answers",
            items_found: found + 1,
            items_total: total.and_then(|n| n.checked_add(1)),
        });
    }
    Some(ex)
}

fn api_text(raw: &str) -> String {
    // Safe API strings are entity-encoded; decode as text then escape once
    // before composing the HTML passed to the normal extraction pipeline.
    let doc = Html::parse_fragment(raw);
    inline::visible_text(doc.root_element())
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn api_post(post: &serde_json::Value, html: &mut String) {
    let author = api_text(
        post["owner"]["display_name"]
            .as_str()
            .unwrap_or("anonymous"),
    );
    let license = api_text(post["content_license"].as_str().unwrap_or(""));
    html.push_str(&format!(
        "<p>{} points · {author} · {license}</p>",
        post["score"].as_i64().unwrap_or(0)
    ));
    html.push_str(post["body"].as_str().unwrap_or(""));
    if let Some(comments) = post["comments"].as_array() {
        for comment in comments.iter().take(100) {
            if let Some(body) = comment["body"].as_str() {
                let author = api_text(
                    comment["owner"]["display_name"]
                        .as_str()
                        .unwrap_or("anonymous"),
                );
                html.push_str(&format!("<p>Comment by {author}: {body}</p>"));
            }
        }
    }
}

fn text_of(el: ElementRef) -> String {
    // Visible text only: a script/style subtree inside the element is
    // source, not content (#288).
    inline::visible_text_raw(el).trim().to_string()
}

fn score_of(post: &ElementRef) -> i64 {
    if let Some(s) = post.value().attr("data-score")
        && let Ok(n) = s.parse()
    {
        return n;
    }
    post.select(&Selector::parse("div.js-vote-count, span.vote-count-post").unwrap())
        .next()
        .map(|s| text_of(s))
        .and_then(|t| t.replace(",", "").parse().ok())
        .unwrap_or(0)
}

/// (score, body-md, author, date) for a question or answer post.
fn post_parts(post: &ElementRef) -> Option<(i64, String, String, String)> {
    let score = score_of(post);
    let body_sel = Selector::parse("div.js-post-body, div.post-text").ok()?;
    let body = post.select(&body_sel).next()?;
    let opts = ExtractOptions {
        include_links: true, // links to fiddles/docs are content here
        ..ExtractOptions::default()
    };
    let md = crate::extract::inline::markdown(body, "https://stackoverflow.com", &opts).0;
    // The OWNER signature is the asker/answerer; edited-by blocks
    // and "modified" stamps would otherwise win as first matches.
    let owner_sel = Selector::parse(".post-signature.owner").unwrap();
    let owner = post.select(&owner_sel).next();
    let author = owner
        .and_then(|o| {
            o.select(&Selector::parse(".user-details a").unwrap())
                .next()
        })
        .map(text_of)
        .filter(|a| !a.is_empty())
        .unwrap_or_default();
    // "asked <span title='2014-04-25 12:45:54Z' class='relativetime'>"
    // : the title attr is ISO; fall back to visible text.
    let date = owner
        .and_then(|o| {
            o.select(&Selector::parse("span[title], span.relativetime").unwrap())
                .next()
        })
        .map(|t| match t.value().attr("title") {
            Some(ts) if ts.starts_with(|c: char| c.is_ascii_digit()) => {
                ts.chars().take(10).collect::<String>()
            }
            _ => text_of(t),
        })
        .or_else(|| {
            post.select(&Selector::parse("time[itemprop='dateCreated']").unwrap())
                .next()
                .and_then(|t| t.value().attr("datetime").map(String::from))
                .map(|dt| dt.chars().take(10).collect())
        })
        .unwrap_or_default();
    Some((score, md, author, date))
}

#[cfg(test)]
mod tests {
    #[test]
    fn report_audit_public_api_keeps_question_answers_comments_and_honest_cuts() {
        let source = "https://stackoverflow.com/questions/123/question";
        let endpoint = api_url(&url::Url::parse(source).unwrap()).unwrap();
        let payload = serde_json::json!({"items":[{
            "question_id":123,"title":"Question &amp; answers", "link":source,
            "body":"<p>Question about the question mark operator.</p><pre><code>let value = option?;</code></pre>",
            "score":10,"owner":{"display_name":"Alice"},"content_license":"CC BY-SA 4.0", "answer_count":1,
            "comments":[{"body":"A useful clarification", "owner":{"display_name":"Carol"}}],
            "answers":[{"answer_id":456,"question_id":123,"is_accepted":true,"score":25,"owner":{"display_name":"Bob"},"content_license":"CC BY-SA 4.0", "body":"<p>The operator returns early when the result is an error.</p><pre><code>value?;</code></pre>"}]
        }], "has_more":false});
        let bytes = serde_json::to_vec(&payload).unwrap();
        assert!(api_payload_valid(&bytes, &endpoint));
        let ex = extract_api(&bytes, &endpoint, &ExtractOptions::default()).unwrap();
        for text in [
            "Question & answers",
            "option?",
            "returns early",
            "ACCEPTED",
            "Alice",
            "Bob",
            "Carol",
            "clarification",
            "CC BY-SA 4.0",
        ] {
            assert!(
                ex.markdown.contains(text),
                "missing {text}: {}",
                ex.markdown
            );
        }
        assert!(ex.partial.is_none());
        let selected = extract_api(
            &bytes,
            &endpoint,
            &ExtractOptions {
                section: Some("Answer 1".into()),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(selected.markdown.contains("returns early"));
        assert!(!selected.markdown.contains("option?"));
        let capped = extract_api(
            &bytes,
            &endpoint,
            &ExtractOptions {
                max_chars: Some(200),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(capped.markdown.len() <= 200 && capped.next_offset.is_some());
        let probe = extract_api(
            &bytes,
            &endpoint,
            &ExtractOptions {
                must_contain: Some("returns early".into()),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(probe.markdown.contains("MATCH") && probe.markdown.len() < ex.markdown.len());
        let mut subset = payload.clone();
        subset["items"][0]["answer_count"] = serde_json::json!(5);
        let partial = extract_api(
            &serde_json::to_vec(&subset).unwrap(),
            &endpoint,
            &ExtractOptions::default(),
        )
        .unwrap()
        .partial
        .unwrap();
        assert_eq!(partial.items_found, 2);
        assert_eq!(partial.items_total, Some(6));
        for bad in [
            serde_json::json!({"items":[]}),
            serde_json::json!({"error_id":502,"items":payload["items"]}),
            serde_json::json!({"items":[{"question_id":999}]}),
        ] {
            assert!(!api_payload_valid(
                &serde_json::to_vec(&bad).unwrap(),
                &endpoint
            ));
        }
        for url in [
            "https://stackoverflow.com/questions/tagged/rust",
            "https://stackoverflow.com.evil.example/questions/123",
            "https://meta.stackoverflow.com/questions/123",
        ] {
            assert!(api_url(&url::Url::parse(url).unwrap()).is_none());
        }
        record_api_backoff(br#"{"backoff":60}"#);
        assert!(
            api_url(&url::Url::parse(source).unwrap()).is_none(),
            "honor service backoff by using the full website path"
        );
        assert!(
            api_payload_valid(&bytes, &endpoint),
            "backoff cannot discard the reply already obtained"
        );
    }

    use super::*;

    fn opts() -> ExtractOptions {
        ExtractOptions::default()
    }

    const QA: &str = r#"<html><body>
      <a class="question-hyperlink" href="/q/1">How do I reverse a Vec in Rust?</a>
      <div class="question" id="question">
        <div class="js-vote-count">123</div>
        <div class="js-post-body"><p>Given <code>Vec&lt;u8&gt;</code> how do I reverse it?</p></div>
        <div class="post-signature owner"><div class="user-action-time">asked <span title="2024-05-01 10:00:00Z" class="relativetime">May 1, 2024</span></div><div class="user-details"><a>tama</a></div></div>
      </div>
      <div class="answer accepted-answer">
        <div class="js-vote-count">200</div>
        <div class="js-post-body"><p>Use <code>v.reverse()</code> in place.</p></div>
        <div class="post-signature owner"><div class="user-details"><a>shep</a></div></div><span class="relativetime" title="2024-05-01T12:00:00Z">May 1</span>
      </div>
      <div class="answer">
        <div class="js-vote-count">50</div>
        <div class="js-post-body"><p>Or <code>v.iter().rev()</code>.</p></div>
        <div class="post-signature owner"><div class="user-details"><a>kai</a></div></div><span class="relativetime" title="2024-05-02 09:00:00Z">May 2</span>
      </div>
    </body></html>"#;

    #[test]
    fn qa_tree_renders() {
        let ex = extract(
            QA,
            "https://stackoverflow.com/questions/1/how-reverse",
            &opts(),
        )
        .unwrap();
        assert_eq!(ex.via, Some("adapter:stackexchange"));
        assert!(ex.markdown.contains("How do I reverse a Vec in Rust?"));
        assert!(ex.markdown.contains("Q · 123 pts · u/tama · 2024-05-01"));
        assert!(ex.markdown.contains("✓ ACCEPTED"));
        assert!(ex.markdown.contains("A1 · 200 pts"));
        assert!(ex.markdown.contains("A2 · 50 pts · u/kai"));
        assert!(ex.markdown.contains("v.reverse()"));
    }

    #[test]
    fn subdomains_and_lists_rejected() {
        assert!(extract(QA, "https://rust.stackexchange.com/questions/1/x", &opts()).is_some());
        // meta.* subdomains and mathoverflow.net are the same
        // platform; look-alike domains are not the network.
        assert!(extract(QA, "https://meta.stackoverflow.com/questions/1/x", &opts()).is_some());
        assert!(extract(QA, "https://mathoverflow.net/questions/1/x", &opts()).is_some());
        assert!(extract(QA, "https://notstackoverflow.com/questions/1/x", &opts()).is_none());
        assert!(
            extract(
                QA,
                "https://stackoverflow.com/questions/tagged/rust",
                &opts()
            )
            .is_none()
        );
        assert!(extract(QA, "https://example.com/questions/1", &opts()).is_none());
    }
}
