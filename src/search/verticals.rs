//! Verticals : keyless JSON APIs with near-100%
//! reliability. They feed the SAME merge as web engines:
//! a GitHub repo that also ranks in Brave gets consensus.

use serde_json::Value;

use super::engines::Hit;
use crate::error::FetchError;
use crate::fetch::client::Fetcher;

fn q(query: &str) -> String {
    url::form_urlencoded::byte_serialize(query.as_bytes()).collect()
}

pub fn endpoint(vertical: &str, query: &str) -> Option<String> {
    let query = q(query);
    match vertical {
        "wikipedia" => Some(format!(
            "https://en.wikipedia.org/w/api.php?action=query&list=search&srsearch={query}&format=json&srlimit=5"
        )),
        "hn" => Some(format!(
            "https://hn.algolia.com/api/v1/search?query={query}&hitsPerPage=5"
        )),
        "stackexchange" => Some(format!(
            "https://api.stackexchange.com/2.3/search/advanced?order=desc&sort=relevance&q={query}&site=stackoverflow&pagesize=5"
        )),
        "mdn" => Some(format!(
            "https://developer.mozilla.org/api/v1/search?q={query}"
        )),
        "github" => {
            let lower = query.to_lowercase().replace('+', " ");
            let words = lower.split_whitespace().count();
            let errorish = [
                "error",
                "exception",
                "failed",
                "bug",
                "crash",
                "panic",
                "cannot",
                "undefined",
            ]
            .iter()
            .any(|s| lower.contains(s));
            let repoish = [
                "library",
                "crate",
                "framework",
                "plugin",
                "sdk",
                "cli",
                "tool",
                "vs code",
                "extension",
            ]
            .iter()
            .any(|s| lower.contains(s))
                || words <= 4;
            // Natural-language how-tos return spam repos from
            // GitHub's loose matcher : skip the vertical.
            if !errorish && !repoish {
                return None;
            }
            // Error-ish queries belong on issue search;
            // repo-ish on repository search.
            if errorish {
                Some(format!(
                    "https://api.github.com/search/issues?q={query}&per_page=5"
                ))
            } else {
                Some(format!(
                    "https://api.github.com/search/repositories?q={query}&per_page=5"
                ))
            }
        }
        "scholar" => Some(format!(
            "https://api.semanticscholar.org/graph/v1/paper/search?query={query}&limit=5&fields=title,url,abstract,year"
        )),
        "news" => Some(format!(
            "https://news.google.com/rss/search?q={query}&hl=en-US&gl=US&ceid=US:en"
        )),
        "arxiv" => Some(format!(
            "https://export.arxiv.org/api/query?search_query=all:{query}&max_results=5&sortBy=relevance"
        )),
        _ => None,
    }
}

/// Verticals call friendly APIs direct by default; the
/// retry wave passes a proxy when direct got rate-limited.
pub async fn run(
    fetcher: &Fetcher,
    vertical: &str,
    query: &str,
    proxy: Option<&crate::egress::proxy::Proxy>,
) -> Result<Vec<Hit>, FetchError> {
    let Some(url) = endpoint(vertical, query) else {
        return Ok(Vec::new());
    };
    let out = fetcher
        .fetch_once_via(&url, &[], proxy, false, None)
        .await?;
    if out.status != 200 {
        return Err(FetchError::Http(format!(
            "{vertical}: status {}",
            out.status
        )));
    }
    let body = String::from_utf8_lossy(&out.body);
    Ok(match vertical {
        "news" => parse_rss(&body),
        "arxiv" => parse_arxiv(&body),
        _ => parse_json(vertical, &body),
    })
}

/// Body → hits router, shared by run() and tests.
pub fn parse(vertical: &str, body: &str) -> Vec<Hit> {
    match vertical {
        "news" => parse_rss(body),
        "arxiv" => parse_arxiv(body),
        _ => parse_json(vertical, body),
    }
}

/// RFC-822-ish date from RSS pubDate -> ISO date string.
/// "Thu, 31 Jul 2026 07:00:00 GMT" -> "2026-07-31".
pub fn rss_date_to_iso(date: &str) -> Option<String> {
    let parts: Vec<&str> = date.split_whitespace().collect();
    if parts.len() < 4 {
        return None;
    }
    let month = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ]
    .iter()
    .position(|m| parts[2].starts_with(m))?;
    Some(format!("{}-{:02}-{}", parts[3], month + 1, parts[1]))
}

fn parse_json(vertical: &str, body: &str) -> Vec<Hit> {
    let Ok(v) = serde_json::from_str::<Value>(body) else {
        return Vec::new();
    };
    match vertical {
        "wikipedia" => v["query"]["search"]
            .as_array()
            .map(|a| {
                a.iter()
                    .enumerate()
                    .map(|(rank, it)| Hit {
                        title: it["title"].as_str().unwrap_or("").to_string(),
                        url: format!(
                            "https://en.wikipedia.org/wiki/{}",
                            it["title"].as_str().unwrap_or("").replace(' ', "_")
                        ),
                        snippet: strip_tags(it["snippet"].as_str().unwrap_or("")),
                        rank,
                        published: None,
                    })
                    .collect()
            })
            .unwrap_or_default(),
        "hn" => v["hits"]
            .as_array()
            .map(|a| {
                a.iter()
                    .enumerate()
                    .filter_map(|(rank, it)| {
                        let url = it["url"].as_str().or(it["story_url"].as_str())?;
                        let title = it["title"].as_str().unwrap_or("");
                        let points = it["points"].as_i64().unwrap_or(0);
                        let comments = it["num_comments"].as_i64().unwrap_or(0);
                        Some(Hit {
                            title: title.to_string(),
                            url: url.to_string(),
                            snippet: format!("Hacker News: {points} points, {comments} comments"),
                            rank,
                            published: None,
                        })
                    })
                    .collect()
            })
            .unwrap_or_default(),
        "github" => v["items"]
            .as_array()
            .map(|a| {
                a.iter()
                    .enumerate()
                    .map(|(rank, it)| {
                        if it["stargazers_count"].is_number() {
                            // repository result
                            let stars = it["stargazers_count"].as_i64().unwrap_or(0);
                            let desc = it["description"].as_str().unwrap_or("");
                            Hit {
                                title: it["full_name"].as_str().unwrap_or("").to_string(),
                                url: it["html_url"].as_str().unwrap_or("").to_string(),
                                snippet: format!("{desc} (★ {stars})"),
                                rank,
                                published: None,
                            }
                        } else {
                            // issue result
                            let repo = it["repository_url"]
                                .as_str()
                                .unwrap_or("")
                                .replace("https://api.github.com/repos/", "");
                            let state = it["state"].as_str().unwrap_or("");
                            Hit {
                                title: format!("{repo}: {}", it["title"].as_str().unwrap_or("")),
                                url: it["html_url"].as_str().unwrap_or("").to_string(),
                                snippet: format!("GitHub issue ({state})"),
                                rank,
                                published: None,
                            }
                        }
                    })
                    .collect()
            })
            .unwrap_or_default(),
        "stackexchange" => v["items"]
            .as_array()
            .map(|a| {
                a.iter()
                    .enumerate()
                    .map(|(rank, it)| {
                        let score = it["score"].as_i64().unwrap_or(0);
                        let answers = it["answer_count"].as_i64().unwrap_or(0);
                        let answered = it["is_answered"].as_bool().unwrap_or(false);
                        Hit {
                            title: it["title"].as_str().unwrap_or("").to_string(),
                            url: it["link"].as_str().unwrap_or("").to_string(),
                            snippet: format!(
                                "Stack Overflow: score {score}, {answers} answers{}",
                                if answered { ", accepted answer" } else { "" }
                            ),
                            rank,
                            published: None,
                        }
                    })
                    .collect()
            })
            .unwrap_or_default(),
        "mdn" => v["documents"]
            .as_array()
            .map(|a| {
                a.iter()
                    .enumerate()
                    .map(|(rank, it)| Hit {
                        title: it["title"].as_str().unwrap_or("").to_string(),
                        url: format!(
                            "https://developer.mozilla.org{}",
                            it["mdn_url"].as_str().unwrap_or("")
                        ),
                        snippet: it["summary"].as_str().unwrap_or("").to_string(),
                        rank,
                        published: None,
                    })
                    .collect()
            })
            .unwrap_or_default(),
        "scholar" => v["data"]
            .as_array()
            .map(|a| {
                a.iter()
                    .enumerate()
                    .map(|(rank, it)| {
                        let year = it["year"]
                            .as_i64()
                            .map(|y| y.to_string())
                            .unwrap_or_default();
                        let abs: String = it["abstract"]
                            .as_str()
                            .unwrap_or("")
                            .chars()
                            .take(220)
                            .collect();
                        Hit {
                            title: it["title"].as_str().unwrap_or("").to_string(),
                            url: it["url"].as_str().unwrap_or("").to_string(),
                            snippet: format!("{abs} ({year})"),
                            rank,
                            published: None,
                        }
                    })
                    .collect()
            })
            .unwrap_or_default(),
        _ => Vec::new(),
    }
}

/// Google News RSS: loose parse (items are well-formed
/// enough for tag scanning; no XML dep needed).
fn parse_rss(body: &str) -> Vec<Hit> {
    let mut hits = Vec::new();
    for (rank, item) in body.split("<item>").skip(1).enumerate() {
        if rank >= 8 {
            break;
        }
        let grab = |tag: &str| -> String {
            let open = format!("<{tag}>");
            let close = format!("</{tag}>");
            item.split_once(&open)
                .and_then(|(_, rest)| rest.split_once(&close))
                .map(|(inner, _)| {
                    inner
                        .trim_start_matches("<![CDATA[")
                        .trim_end_matches("]]>")
                        .to_string()
                })
                .unwrap_or_default()
        };
        let title = grab("title");
        let url = grab("link");
        let date = grab("pubDate");
        let iso = rss_date_to_iso(&date);
        if !title.is_empty() && url.starts_with("http") {
            // Google News titles end in " - Publisher"; a date
            // alone is a snippet that says nothing about the
            // story, so carry publisher + date instead.
            let publisher = title
                .rsplit_once(" - ")
                .map(|(_, p)| p.trim())
                .unwrap_or("")
                .to_string();
            let snippet = if publisher.is_empty() {
                date.clone()
            } else {
                format!("{publisher} · {date}")
            };
            hits.push(Hit {
                title,
                url,
                snippet,
                rank,
                published: iso,
            });
        }
    }
    hits
}

/// arXiv Atom feed: loose tag scan like the RSS parser.
fn parse_arxiv(body: &str) -> Vec<Hit> {
    let mut hits = Vec::new();
    for (rank, entry) in body.split("<entry>").skip(1).enumerate() {
        if rank >= 5 {
            break;
        }
        let grab = |tag: &str| -> String {
            let open = format!("<{tag}>");
            let close = format!("</{tag}>");
            entry
                .split_once(&open)
                .and_then(|(_, rest)| rest.split_once(&close))
                .map(|(inner, _)| inner.trim().to_string())
                .unwrap_or_default()
        };
        let title = grab("title")
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        // arxiv id is the canonical abs-page URL.
        let url = grab("id");
        let summary: String = grab("summary")
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .chars()
            .take(220)
            .collect();
        let published = grab("published").chars().take(10).collect::<String>();
        if !title.is_empty() && url.starts_with("http") {
            hits.push(Hit {
                title,
                url,
                snippet: summary,
                rank,
                published: Some(published),
            });
        }
    }
    hits
}

fn strip_tags(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_tag = false;
    for c in s.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}
