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
