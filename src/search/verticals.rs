//! Verticals : keyless JSON APIs with near-100%
//! reliability. They feed the SAME merge as web engines:
//! a GitHub repo that also ranks in Brave gets consensus.

use std::time::Duration;

use serde_json::Value;

use super::engines::Hit;
use crate::error::FetchError;
use crate::fetch::client::Fetcher;

/// How a first-party interface said no. A throttle is not a failure of the
/// source: it is the source publishing a limit, and the right answer is to stop
/// asking until it lapses, not to retry and not to distrust the source.
#[derive(Debug)]
pub enum VerticalError {
    /// The interface asked for time. Carries how long.
    Throttled(Duration),
    Fetch(FetchError),
}

impl From<FetchError> for VerticalError {
    fn from(e: FetchError) -> Self {
        VerticalError::Fetch(e)
    }
}

/// How long to rest a throttled source when it did not say.
const DEFAULT_REST: Duration = Duration::from_secs(900);
/// A source that asks for longer than this is rested this long and re-probed:
/// a stale daily quota should not bench it for a day on one reading.
const MAX_REST: Duration = Duration::from_secs(3600);

fn q(query: &str) -> String {
    url::form_urlencoded::byte_serialize(query.as_bytes()).collect()
}

pub fn endpoint(vertical: &str, raw: &str) -> Option<String> {
    let query = q(raw);
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
        "arxiv" => {
            // A query that reads as a paper's title is looked up as one: every
            // word must be in the title, which finds the paper itself where
            // `all:` over abstracts finds papers that merely discuss it. `run`
            // credits such an answer only when a returned title covers the query.
            let field = match title_terms(raw).and_then(|t| indexed_title_terms(&t)) {
                Some(t) => t
                    .iter()
                    .map(|w| format!("ti:{w}"))
                    .collect::<Vec<_>>()
                    .join("+AND+"),
                None => format!("all:{query}"),
            };
            Some(format!(
                "https://export.arxiv.org/api/query?search_query={field}&max_results=5&sortBy=relevance"
            ))
        }
        "arxiv-all" => Some(format!(
            "https://export.arxiv.org/api/query?search_query=all:{query}&max_results=5&sortBy=relevance"
        )),
        "openalex" => Some(format!(
            "https://api.openalex.org/works?search={query}&per-page=5&select=id,doi,title,publication_year,cited_by_count,primary_location"
        )),
        "europepmc" => Some(format!(
            "https://www.ebi.ac.uk/europepmc/webservices/rest/search?query={query}&format=json&pageSize=5&resultType=lite"
        )),
        "crates" => {
            let terms = package_terms(raw)?;
            Some(format!(
                "https://crates.io/api/v1/crates?q={}&per_page=5",
                q(&terms)
            ))
        }
        "npm" => {
            let terms = package_terms(raw)?;
            Some(format!(
                "https://registry.npmjs.org/-/v1/search?text={}&size=5",
                q(&terms)
            ))
        }
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
) -> Result<Vec<Hit>, VerticalError> {
    let Some(url) = endpoint(vertical, query) else {
        return Ok(Vec::new());
    };
    let out = fetcher
        .fetch_once_via(&url, &[], proxy, false, None)
        .await?;
    if let Some(rest) = throttle(vertical, out.status, &out.headers, &out.body) {
        return Err(VerticalError::Throttled(rest));
    }
    if out.status != 200 {
        return Err(VerticalError::Fetch(FetchError::Http(format!(
            "{vertical}: status {}",
            out.status
        ))));
    }
    let body = String::from_utf8_lossy(&out.body);
    let hits = parse(vertical, &body);
    if vertical == "arxiv" && title_terms(query).is_some() {
        // The title lookup is an exact-ish contract: it is credited only for
        // returned titles that cover the query. When none does, the query was
        // not a title, so ask the way an ordinary query is asked.
        let mut covered: Vec<Hit> = hits
            .into_iter()
            .filter(|h| title_covers(query, &h.title))
            .collect();
        // The title that holds the most of the query leads; ties keep arXiv's order.
        covered
            .sort_by(|a, b| title_cover(query, &b.title).total_cmp(&title_cover(query, &a.title)));
        for (rank, h) in covered.iter_mut().enumerate() {
            h.rank = rank;
        }
        if !covered.is_empty() {
            return Ok(covered);
        }
        let Some(url) = endpoint("arxiv-all", query) else {
            return Ok(Vec::new());
        };
        let out = fetcher
            .fetch_once_via(&url, &[], proxy, false, None)
            .await?;
        if let Some(rest) = throttle(vertical, out.status, &out.headers, &out.body) {
            return Err(VerticalError::Throttled(rest));
        }
        if out.status != 200 {
            return Err(VerticalError::Fetch(FetchError::Http(format!(
                "{vertical}: status {}",
                out.status
            ))));
        }
        return Ok(parse("arxiv", &String::from_utf8_lossy(&out.body)));
    }
    Ok(hits)
}

const QUESTION_WORDS: &[&str] = &[
    "how",
    "why",
    "what",
    "when",
    "which",
    "where",
    "who",
    "best",
    "latest",
    "recent",
    "new",
    "top",
    "vs",
    "versus",
    "survey",
    "review",
    "tutorial",
    "explained",
    "explain",
];

fn words(s: &str) -> Vec<String> {
    s.split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_lowercase)
        .collect()
}

/// The words of a query that reads as a paper's title: three to twelve of them,
/// no question or comparison words, nothing outside ASCII (the terms go into a
/// URL as written). `None` for any other query. A false positive costs one
/// extra request, because `run` falls back to the plain search when no returned
/// title covers the query; a false negative is the plain search as before.
pub(crate) fn title_terms(raw: &str) -> Option<Vec<String>> {
    const NOISE: &[&str] = &["paper", "arxiv", "pdf"];
    let all = words(raw);
    let terms: Vec<String> = all
        .into_iter()
        .filter(|w| !NOISE.contains(&w.as_str()))
        .collect();
    let ok = (3..=12).contains(&terms.len())
        && terms.iter().all(|w| w.is_ascii())
        && !terms.iter().any(|w| QUESTION_WORDS.contains(&w.as_str()));
    ok.then_some(terms)
}

/// The title terms arXiv's title index can be asked for. It does not index
/// function words, and an AND with one returns nothing (`ti:is AND ti:all`
/// finds no "Attention Is All You Need"), so they are left out of the request
/// and judged afterwards by [`title_covers`]. `None` when under two words are
/// left: too little to find a title with.
fn indexed_title_terms(terms: &[String]) -> Option<Vec<String>> {
    const FUNCTION_WORDS: &[&str] = &[
        "is", "are", "all", "you", "your", "a", "an", "the", "of", "for", "and", "or", "in", "on",
        "to", "with", "at", "by", "as", "from", "via", "it", "its", "be", "not", "no", "we",
        "that", "this",
    ];
    let kept: Vec<String> = terms
        .iter()
        .filter(|w| !FUNCTION_WORDS.contains(&w.as_str()))
        .cloned()
        .collect();
    (kept.len() >= 2).then_some(kept)
}

/// What share of the query's words a title holds, in tenths of a percent-free
/// ratio. Zero for a query that is not title-like.
fn title_cover(query: &str, title: &str) -> f64 {
    let Some(terms) = title_terms(query) else {
        return 0.0;
    };
    let in_title: std::collections::HashSet<String> = words(title).into_iter().collect();
    terms.iter().filter(|t| in_title.contains(*t)).count() as f64 / terms.len() as f64
}

/// A returned title covers the query when it holds at least 60% of the query's
/// words: enough for a title typed loosely ("resnet" for "Deep Residual
/// Learning ..."), too much for a paper that merely mentions the topic.
pub(crate) fn title_covers(query: &str, title: &str) -> bool {
    title_cover(query, title) >= 0.6
}

/// The query for a package registry: the first two words that name the package,
/// without the words that name the registry or the kind of thing it is. A
/// registry matches names, so descriptive words ("error handling") push the
/// package itself out of its own result list; the name comes first in how
/// people write these queries.
pub(crate) fn package_terms(raw: &str) -> Option<String> {
    const QUALIFIERS: &[&str] = &[
        "rust",
        "crate",
        "crates",
        "cargo",
        "npm",
        "package",
        "packages",
        "library",
        "lib",
        "js",
        "javascript",
        "typescript",
        "node",
        "nodejs",
        "install",
        "docs",
        "documentation",
        "yarn",
        "pnpm",
        "how",
        "to",
        "use",
        "using",
        "a",
        "the",
        "for",
        "in",
        "with",
    ];
    let kept: Vec<String> = words(raw)
        .into_iter()
        .filter(|w| !QUALIFIERS.contains(&w.as_str()))
        .take(2)
        .collect();
    (!kept.is_empty()).then(|| kept.join(" "))
}

/// Did this response say "come back later"? Each interface publishes its limit
/// in its own dialect, and each one is a contract this reads rather than a
/// failure it guesses at: a plain 429 with an optional `Retry-After`, GitHub's
/// `x-ratelimit-remaining: 0` on a 403, and Stack Exchange's throttle, which
/// arrives as HTTP 400 with a JSON error body (`error_id` 502) once an IP has
/// spent its daily quota.
pub(crate) fn throttle(
    vertical: &str,
    status: u16,
    headers: &[(String, String)],
    body: &[u8],
) -> Option<Duration> {
    let header = |name: &str| {
        headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.trim().to_string())
    };
    let clamp = |d: Duration| d.clamp(Duration::from_secs(60), MAX_REST);
    if status == 429 {
        let wait = header("retry-after")
            .and_then(|v| v.parse::<u64>().ok())
            .map_or(DEFAULT_REST, Duration::from_secs);
        return Some(clamp(wait));
    }
    if status == 403 && header("x-ratelimit-remaining").as_deref() == Some("0") {
        let wait = header("x-ratelimit-reset")
            .and_then(|v| v.parse::<u64>().ok())
            .and_then(|reset| {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .ok()?
                    .as_secs();
                Some(Duration::from_secs(reset.saturating_sub(now)))
            })
            .unwrap_or(DEFAULT_REST);
        return Some(clamp(wait));
    }
    if vertical == "stackexchange" && status == 400 {
        let v: Value = serde_json::from_slice(body).ok()?;
        let throttled = v["error_id"].as_i64() == Some(502)
            || v["error_name"].as_str() == Some("throttle_violation");
        if throttled {
            // The message names the wait ("... Please wait 78310 seconds"); an
            // unreadable one rests the source for the default.
            let wait = v["error_message"]
                .as_str()
                .and_then(|m| {
                    m.split_whitespace()
                        .zip(m.split_whitespace().skip(1))
                        .find(|(_, next)| next.starts_with("second"))
                        .and_then(|(n, _)| n.parse::<u64>().ok())
                })
                .map_or(DEFAULT_REST, Duration::from_secs);
            return Some(clamp(wait));
        }
    }
    None
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
        "openalex" => parse_openalex(&v),
        "europepmc" => parse_europepmc(&v),
        "crates" => parse_crates(&v),
        "npm" => parse_npm(&v),
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

fn parse_openalex(v: &Value) -> Vec<Hit> {
    v["results"]
        .as_array()
        .map(|a| {
            a.iter()
                .enumerate()
                .filter_map(|(rank, it)| {
                    let title = it["title"].as_str()?.trim().to_string();
                    let landing = it["primary_location"]["landing_page_url"]
                        .as_str()
                        .filter(|u| u.starts_with("http"));
                    let url = landing
                        .or_else(|| it["doi"].as_str().filter(|u| u.starts_with("http")))
                        .or_else(|| it["id"].as_str())?
                        .to_string();
                    let year = it["publication_year"].as_i64().unwrap_or(0);
                    let cited = it["cited_by_count"].as_i64().unwrap_or(0);
                    Some(Hit {
                        title,
                        url,
                        snippet: format!("OpenAlex: {year}, {cited} citations"),
                        rank,
                        published: None,
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Europe PMC's record id has no URL; each source has its own home for it.
fn europepmc_url(source: &str, id: &str) -> Option<String> {
    match source {
        "MED" => Some(format!("https://pubmed.ncbi.nlm.nih.gov/{id}/")),
        "PPR" => Some(format!("https://europepmc.org/article/PPR/{id}")),
        "PMC" => Some(format!("https://pmc.ncbi.nlm.nih.gov/articles/{id}/")),
        _ => None,
    }
}

fn parse_europepmc(v: &Value) -> Vec<Hit> {
    v["resultList"]["result"]
        .as_array()
        .map(|a| {
            a.iter()
                .enumerate()
                .filter_map(|(rank, it)| {
                    let title = it["title"]
                        .as_str()?
                        .trim()
                        .trim_end_matches('.')
                        .to_string();
                    let url = europepmc_url(it["source"].as_str()?, it["id"].as_str()?)?;
                    let authors = it["authorString"].as_str().unwrap_or("");
                    let year = it["pubYear"].as_str().unwrap_or("");
                    let journal = it["journalTitle"].as_str().unwrap_or("");
                    Some(Hit {
                        title,
                        url,
                        snippet: format!("{authors} ({year}) {journal}").trim().to_string(),
                        rank,
                        published: None,
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

fn parse_crates(v: &Value) -> Vec<Hit> {
    v["crates"]
        .as_array()
        .map(|a| {
            a.iter()
                .enumerate()
                .filter_map(|(rank, it)| {
                    let name = it["name"].as_str()?;
                    let version = it["max_version"].as_str().unwrap_or("");
                    let desc = it["description"].as_str().unwrap_or("").trim();
                    let downloads = it["downloads"].as_i64().unwrap_or(0);
                    Some(Hit {
                        title: format!("{name} {version}").trim().to_string(),
                        url: format!("https://crates.io/crates/{name}"),
                        snippet: format!("{desc} ({downloads} downloads)"),
                        rank,
                        published: None,
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

fn parse_npm(v: &Value) -> Vec<Hit> {
    v["objects"]
        .as_array()
        .map(|a| {
            a.iter()
                .enumerate()
                .filter_map(|(rank, it)| {
                    let pkg = &it["package"];
                    let name = pkg["name"].as_str()?;
                    let version = pkg["version"].as_str().unwrap_or("");
                    let desc = pkg["description"].as_str().unwrap_or("").trim();
                    Some(Hit {
                        title: format!("{name} {version}").trim().to_string(),
                        url: pkg["links"]["npm"].as_str().map_or_else(
                            || format!("https://www.npmjs.com/package/{name}"),
                            str::to_string,
                        ),
                        snippet: desc.to_string(),
                        rank,
                        published: None,
                    })
                })
                .collect()
        })
        .unwrap_or_default()
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

#[cfg(test)]
mod tests {
    use super::*;

    fn secs(s: u64) -> Option<Duration> {
        Some(Duration::from_secs(s))
    }

    #[test]
    fn stack_exchange_quota_is_a_throttle_with_its_own_wait() {
        let body = br#"{"error_id":502,"error_message":"too many requests from this IP, more requests available in 78310 seconds","error_name":"throttle_violation"}"#;
        // Clamped: a day-long wait rests the source an hour and is re-read then.
        assert_eq!(throttle("stackexchange", 400, &[], body), secs(3600));
        let short = br#"{"error_id":502,"error_message":"Please wait 120 seconds","error_name":"throttle_violation"}"#;
        assert_eq!(throttle("stackexchange", 400, &[], short), secs(120));
        let unreadable = br#"{"error_id":502,"error_name":"throttle_violation"}"#;
        assert_eq!(throttle("stackexchange", 400, &[], unreadable), secs(900));
    }

    #[test]
    fn an_ordinary_bad_request_is_not_a_throttle() {
        let body =
            br#"{"error_id":400,"error_message":"bad parameter","error_name":"bad_parameter"}"#;
        assert_eq!(throttle("stackexchange", 400, &[], body), None);
        assert_eq!(throttle("stackexchange", 400, &[], b"not json"), None);
        // The 502 body is Stack Exchange's dialect; another source's 400 is its own bug.
        let se = br#"{"error_id":502,"error_name":"throttle_violation"}"#;
        assert_eq!(throttle("github", 400, &[], se), None);
        assert_eq!(throttle("github", 404, &[], b""), None);
        assert_eq!(throttle("github", 200, &[], b""), None);
    }

    #[test]
    fn retry_after_and_exhausted_quota_headers_are_read() {
        let h = |n: &str, v: &str| vec![(n.to_string(), v.to_string())];
        assert_eq!(
            throttle("openalex", 429, &h("Retry-After", "300"), b""),
            secs(300)
        );
        assert_eq!(throttle("openalex", 429, &[], b""), secs(900));
        assert_eq!(
            throttle("openalex", 429, &h("retry-after", "5"), b""),
            secs(60)
        );
        assert_eq!(
            throttle("github", 403, &h("x-ratelimit-remaining", "0"), b""),
            secs(900)
        );
        // A 403 that is not a quota stays a plain failure.
        assert_eq!(
            throttle("github", 403, &h("x-ratelimit-remaining", "12"), b""),
            None
        );
        assert_eq!(throttle("github", 403, &[], b""), None);
    }

    #[test]
    fn a_title_shaped_query_is_asked_as_a_title() {
        let url = endpoint("arxiv", "attention is all you need").unwrap();
        assert!(url.contains("ti:attention+AND+ti:need"), "{url}");
        assert!(
            !url.contains("ti:is"),
            "function words are not indexed: {url}"
        );
        let plain = endpoint("arxiv", "how do transformers work").unwrap();
        assert!(plain.contains("all:how"), "{plain}");
        let short = endpoint("arxiv", "resnet").unwrap();
        assert!(short.contains("all:resnet"), "{short}");
    }

    #[test]
    fn a_title_is_credited_only_when_it_covers_the_query() {
        let q = "attention is all you need";
        assert!(title_covers(q, "Attention Is All You Need"));
        assert!(title_covers(
            "deep residual learning image recognition resnet",
            "Deep Residual Learning for Image Recognition"
        ));
        assert!(!title_covers(
            q,
            "Graph Neural Networks for Traffic Forecasting"
        ));
        // Not a title-shaped query: nothing is a title match.
        assert!(!title_covers(
            "how does attention work",
            "How does attention work"
        ));
    }

    #[test]
    fn package_queries_lose_the_words_that_name_the_registry() {
        assert_eq!(package_terms("rust tokio crate").as_deref(), Some("tokio"));
        assert_eq!(
            package_terms("npm install express").as_deref(),
            Some("express")
        );
        assert_eq!(package_terms("rust").as_deref(), None);
        assert_eq!(
            package_terms("rust anyhow error handling crate").as_deref(),
            Some("anyhow error")
        );
        assert!(endpoint("crates", "rust").is_none());
    }

    #[test]
    fn openalex_prefers_the_landing_page_and_skips_untitled() {
        let body = r#"{"results":[
          {"id":"https://openalex.org/W1","doi":"https://doi.org/10.48550/arxiv.1706.03762",
           "title":"Attention Is All You Need","publication_year":2017,"cited_by_count":135111,
           "primary_location":{"landing_page_url":"http://arxiv.org/abs/1706.03762"}},
          {"id":"https://openalex.org/W2","doi":"https://doi.org/10.1/x","title":"Doi Only",
           "publication_year":2020,"cited_by_count":3,"primary_location":null},
          {"id":"https://openalex.org/W3","title":null}]}"#;
        let hits = parse("openalex", body);
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].url, "http://arxiv.org/abs/1706.03762");
        assert_eq!(hits[1].url, "https://doi.org/10.1/x");
        assert!(hits[0].snippet.contains("135111 citations"));
    }

    #[test]
    fn europe_pmc_records_get_the_url_their_source_uses() {
        let body = r#"{"resultList":{"result":[
          {"id":"41794399","source":"MED","title":"CRISPR-Cas9 therapy: expanding possibilities.",
           "authorString":"Adashi EY.","journalTitle":"Am J Med Sci","pubYear":"2026"},
          {"id":"PMC123","source":"PMC","title":"A","pubYear":"2020"},
          {"id":"PPR9","source":"PPR","title":"B"},
          {"id":"1","source":"AGR","title":"unknown source"}]}}"#;
        let hits = parse("europepmc", body);
        let urls: Vec<_> = hits.iter().map(|h| h.url.as_str()).collect();
        assert_eq!(
            urls,
            [
                "https://pubmed.ncbi.nlm.nih.gov/41794399/",
                "https://pmc.ncbi.nlm.nih.gov/articles/PMC123/",
                "https://europepmc.org/article/PPR/PPR9",
            ]
        );
        assert_eq!(
            hits[0].title,
            "CRISPR-Cas9 therapy: expanding possibilities"
        );
    }

    #[test]
    fn crates_and_npm_hits_point_at_the_registry_page() {
        let crates = r#"{"crates":[{"name":"tokio","max_version":"1.54.1","downloads":1165282647,
            "description":"An event-driven, non-blocking I/O platform"}]}"#;
        let c = parse("crates", crates);
        assert_eq!(c[0].url, "https://crates.io/crates/tokio");
        assert_eq!(c[0].title, "tokio 1.54.1");
        let npm = r#"{"objects":[{"package":{"name":"@types/express","version":"5.0.6",
            "links":{"npm":"https://www.npmjs.com/package/@types/express"}}},
            {"package":{"name":"express","version":"5.3.0","description":"Fast"}}]}"#;
        let n = parse("npm", npm);
        assert_eq!(n[0].url, "https://www.npmjs.com/package/@types/express");
        assert_eq!(n[1].url, "https://www.npmjs.com/package/express");
        assert!(parse("npm", "not json").is_empty());
    }
}
