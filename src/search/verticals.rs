//! Native first-party JSON and Atom interfaces. Their records enter the same
//! merge as web results; refusals keep their own status and retry window.

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

// Generated once with /filters/create (base=none). The immutable filter asks
// for question title/link/score/count/accepted-id, nested answer HTML/id/
// accepted/score, and wrapper items/errors/backoff/quota/has_more. No runtime
// filter creation and no second request for the answers.
// Contract: https://api.stackexchange.com/docs/filters and /docs/types/question.
const STACKEXCHANGE_FILTER: &str = "!T.hvPg9V1MIOeLCRH7WlQGxp-JkkGy";

fn q(query: &str) -> String {
    url::form_urlencoded::byte_serialize(query.as_bytes()).collect()
}

/// Only caller-supplied search text reaches an API. Web operators are enforced
/// on candidates, and an intitle value remains searchable text.
fn api_query(raw: &str) -> String {
    let c = super::query::compile(raw);
    match c.intitle {
        Some(title) => format!("{} {}", c.text, title).trim().to_string(),
        None => c.text,
    }
}

pub fn endpoint(vertical: &str, raw: &str) -> Option<String> {
    endpoint_in(vertical, raw, None)
}

/// The endpoint with the query's language written into the source that has an
/// edition per language (Wikipedia), and left alone for the rest.
pub fn endpoint_in(
    vertical: &str,
    raw: &str,
    locale: Option<super::locale::Locale>,
) -> Option<String> {
    let text = api_query(raw);
    let raw = text.as_str();
    let query = q(raw);
    let wiki = locale.map(|l| l.lang).unwrap_or("en");
    match vertical {
        "wikipedia" => Some(format!(
            "https://{wiki}.wikipedia.org/w/api.php?action=query&generator=search&gsrsearch={query}&gsrlimit=5&gsrnamespace=0&prop=extracts%7Cinfo&inprop=url&exintro=1&explaintext=1&exchars=1000&exlimit=5&format=json&formatversion=2"
        )),
        "hn" => Some(format!(
            "https://hn.algolia.com/api/v1/search?query={query}&hitsPerPage=5"
        )),
        "stackexchange" => Some(format!(
            "https://api.stackexchange.com/2.3/search/advanced?order=desc&sort=relevance&q={query}&site=stackoverflow&pagesize=5&filter={STACKEXCHANGE_FILTER}"
        )),
        "mdn" => Some(format!(
            "https://developer.mozilla.org/api/v1/search?q={query}"
        )),
        "github" => {
            let lower = raw.to_lowercase();
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
    locale: Option<super::locale::Locale>,
) -> Result<Vec<Hit>, VerticalError> {
    let text = api_query(query);
    let query = text.as_str();
    let Some(url) = endpoint_in(vertical, query, locale) else {
        return Ok(Vec::new());
    };
    let out = fetcher
        .fetch_once_via(&url, &[], proxy, false, None)
        .await?;
    if let Some(rest) = throttle(vertical, out.status, &out.headers, &out.body) {
        super::record_api_rest(&url, rest);
        return Err(VerticalError::Throttled(rest));
    }
    if out.status != 200 {
        return Err(VerticalError::Fetch(FetchError::Http(format!(
            "{vertical}: status {}",
            out.status
        ))));
    }
    let body = String::from_utf8_lossy(&out.body);
    let hits = if vertical == "stackexchange" {
        let data = stackexchange_data(&body, &url)?;
        parse_stackexchange(&data, Some(query))
    } else if vertical == "wikipedia" {
        wikipedia_response(&body, locale)?
    } else {
        parse_in(vertical, &body, locale)
    };
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
        return Ok(parse_in(
            "arxiv",
            &String::from_utf8_lossy(&out.body),
            locale,
        ));
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
    if matches!(status, 403 | 429 | 503)
        && let Some(value) = header("retry-after")
    {
        let wait = value
            .parse::<u64>()
            .ok()
            .map(Duration::from_secs)
            .or_else(|| {
                httpdate::parse_http_date(&value).ok().map(|date| {
                    date.duration_since(std::time::SystemTime::now())
                        .unwrap_or_default()
                })
            });
        if let Some(wait) = wait {
            return Some(wait.max(Duration::from_secs(1)));
        }
    }
    if status == 429 {
        return Some(DEFAULT_REST);
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
        return Some(wait.max(Duration::from_secs(1)));
    }
    if vertical == "stackexchange" && matches!(status, 200 | 400) {
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
            return Some(wait.max(Duration::from_secs(1)));
        }
    }
    None
}

fn stackexchange_data(body: &str, request_url: &str) -> Result<Value, VerticalError> {
    let fail = || {
        VerticalError::Fetch(FetchError::Http(
            "stackexchange: invalid API response".into(),
        ))
    };
    let data: Value = serde_json::from_str(body).map_err(|_| fail())?;
    if let Some(wait) = stackexchange_wait(&data) {
        super::record_api_rest(request_url, wait);
    }
    if data.get("error_id").is_some() || !data["items"].is_array() {
        return Err(fail());
    }
    Ok(data)
}

/// A 200 response can contain usable records and publish a wait for future
/// requests. Without a supplied reset time, an exhausted daily quota rests for
/// one full day; do not guess the source's reset timezone.
fn stackexchange_wait(data: &Value) -> Option<Duration> {
    let backoff = data["backoff"].as_u64().filter(|n| *n > 0);
    let exhausted = (data["quota_remaining"].as_u64() == Some(0)).then_some(86_400);
    backoff
        .into_iter()
        .chain(exhausted)
        .max()
        .map(Duration::from_secs)
}

fn parse_stackexchange(data: &Value, query: Option<&str>) -> Vec<Hit> {
    let Some(items) = data["items"].as_array() else {
        return Vec::new();
    };
    items
        .iter()
        .take(5)
        .enumerate()
        .filter_map(|(rank, item)| {
            let title = super::engines::decode_xml(item["title"].as_str()?);
            let url = item["link"].as_str()?.to_string();
            let score = item["score"].as_i64().unwrap_or(0);
            let count = item["answer_count"].as_u64().unwrap_or(0);
            let accepted_id = item["accepted_answer_id"].as_u64();
            let accepted = |a: &Value| {
                a["is_accepted"].as_bool() == Some(true)
                    || accepted_id.is_some_and(|id| a["answer_id"].as_u64() == Some(id))
            };
            let answer = item["answers"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|a| a["body"].as_str().is_some_and(|s| !s.trim().is_empty()))
                .filter(|a| accepted(a) || a["score"].as_i64().unwrap_or(0) >= 0)
                .max_by_key(|a| (accepted(a), a["score"].as_i64().unwrap_or(0)));
            let snippet = answer
                .and_then(|a| {
                    let html = a["body"].as_str()?;
                    let options = crate::extract::ExtractOptions {
                        max_chars: Some(64_000),
                        ..Default::default()
                    };
                    let markdown =
                        crate::extract::extract(html.as_bytes(), "text/html", &url, &options)
                            .ok()?
                            .markdown;
                    // The supplied question title is the real context of its answer.
                    let document = format!("# {title}\n\n{markdown}");
                    let label = if accepted(a) {
                        "Accepted answer"
                    } else {
                        "Answer"
                    };
                    let budget = super::evidence::PASSAGE_CHARS.saturating_sub(label.len() + 2);
                    let passage =
                        super::evidence::passage(query.unwrap_or(&title), &document, budget)
                            .unwrap_or_else(|| {
                                super::evidence::cut(
                                    &markdown.split_whitespace().collect::<Vec<_>>().join(" "),
                                    budget,
                                )
                            });
                    (!passage.is_empty()).then(|| format!("{label}: {passage}"))
                })
                .unwrap_or_else(|| {
                    format!(
                        "Stack Overflow: score {score}, {count} answers{}",
                        if accepted_id.is_some() {
                            ", accepted answer"
                        } else {
                            ""
                        }
                    )
                });
            Some(Hit {
                title,
                url,
                snippet,
                rank,
                published: None,
            })
        })
        .collect()
}

/// Body → hits router, shared by run() and tests.
pub fn parse(vertical: &str, body: &str) -> Vec<Hit> {
    parse_in(vertical, body, None)
}

pub fn parse_in(vertical: &str, body: &str, locale: Option<super::locale::Locale>) -> Vec<Hit> {
    match vertical {
        "arxiv" => parse_arxiv(body),
        _ => parse_json(vertical, body, locale),
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

fn wikipedia_response(
    body: &str,
    locale: Option<super::locale::Locale>,
) -> Result<Vec<Hit>, VerticalError> {
    let fail = || VerticalError::Fetch(FetchError::Http("wikipedia: invalid API response".into()));
    let data: Value = serde_json::from_str(body).map_err(|_| fail())?;
    if data.get("error").is_some() {
        return Err(fail());
    }
    if data["query"]["pages"].is_array() {
        return Ok(parse_wikipedia_pages(&data));
    }
    if data["query"]["search"].is_array() {
        return Ok(parse_json("wikipedia", body, locale));
    }
    if data.get("batchcomplete").is_some() && data.get("query").is_none() {
        return Ok(Vec::new());
    }
    Err(fail())
}

fn parse_wikipedia_pages(data: &Value) -> Vec<Hit> {
    let Some(pages) = data["query"]["pages"].as_array() else {
        return Vec::new();
    };
    let mut pages: Vec<_> = pages
        .iter()
        .filter(|p| {
            !p["missing"].as_bool().unwrap_or(false) && !p["invalid"].as_bool().unwrap_or(false)
        })
        .collect();
    pages.sort_by_key(|p| p["index"].as_i64().unwrap_or(i64::MAX));
    pages
        .into_iter()
        .filter_map(|page| {
            let title = page["title"].as_str()?.trim();
            let url = page["fullurl"].as_str()?;
            let parsed = url::Url::parse(url).ok()?;
            let host = parsed.host_str()?;
            if title.is_empty()
                || !matches!(parsed.scheme(), "https" | "http")
                || !host.ends_with(".wikipedia.org")
            {
                return None;
            }
            Some(Hit {
                title: title.to_string(),
                url: url.to_string(),
                snippet: page["extract"].as_str().unwrap_or_default().to_string(),
                rank: 0,
                published: None,
            })
        })
        .take(5)
        .enumerate()
        .map(|(rank, mut hit)| {
            hit.rank = rank;
            hit
        })
        .collect()
}

fn parse_json(vertical: &str, body: &str, locale: Option<super::locale::Locale>) -> Vec<Hit> {
    let Ok(v) = serde_json::from_str::<Value>(body) else {
        return Vec::new();
    };
    match vertical {
        "openalex" => parse_openalex(&v),
        "europepmc" => parse_europepmc(&v),
        "crates" => parse_crates(&v),
        "npm" => parse_npm(&v),
        "wikipedia" if v["query"]["pages"].is_array() => parse_wikipedia_pages(&v),
        // Old captured list-search responses remain readable for replay.
        "wikipedia" => v["query"]["search"]
            .as_array()
            .map(|a| {
                a.iter()
                    .enumerate()
                    .map(|(rank, it)| Hit {
                        title: it["title"].as_str().unwrap_or("").to_string(),
                        url: format!(
                            "https://{}.wikipedia.org/wiki/{}",
                            locale.map(|l| l.lang).unwrap_or("en"),
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
        "stackexchange" => parse_stackexchange(&v, None),
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

    #[test]
    fn wikipedia_extracts_keep_the_api_order_facts_and_canonical_destinations() {
        let body = r#"{"batchcomplete":true,"query":{"pages":[
            {"title":"Second","index":2,"fullurl":"https://en.wikipedia.org/wiki/Actual_second","extract":"A second authentic introductory paragraph."},
            {"title":"First","index":1,"missing":false,"fullurl":"https://en.wikipedia.org/wiki/Canonical_first","extract":"The answering fact, from the page's introductory text."},
            {"title":"Missing","index":0,"missing":true,"fullurl":"https://en.wikipedia.org/wiki/Missing"},
            {"title":"Bad destination","index":3,"fullurl":"https://wikipedia.org.evil.invalid/page"}
        ]}}"#;
        let hits = wikipedia_response(body, None).unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].title, "First");
        assert_eq!(hits[0].url, "https://en.wikipedia.org/wiki/Canonical_first");
        assert!(hits[0].snippet.contains("answering fact"));
        assert_eq!(hits[0].rank, 0);
        assert_eq!(hits[1].rank, 1);
        assert!(
            wikipedia_response(r#"{"batchcomplete":true}"#, None)
                .unwrap()
                .is_empty()
        );
        for invalid in [
            "not JSON",
            r#"{"error":{"code":"maxlag"}}"#,
            r#"{"unexpected":"schema"}"#,
        ] {
            assert!(wikipedia_response(invalid, None).is_err());
        }
    }

    #[tokio::test]
    #[ignore = "native public Wikipedia contract witness; requires network and explicit output path"]
    async fn wikipedia_extract_contract_live_witness() {
        let path =
            std::env::var_os("DSB_SOURCE_WITNESS").expect("set an absolute witness output path");
        let fetcher = Fetcher::new(crate::profile::BrowserProfile::host_default()).unwrap();
        let count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let answer = crate::fetch::core::REQUEST_COUNT
            .scope(
                std::sync::Arc::clone(&count),
                crate::fetch::core::REQUEST_LIMIT.scope(
                    2,
                    run(&fetcher, "wikipedia", "quantum entanglement", None, None),
                ),
            )
            .await;
        let mut receipt = serde_json::json!({"source":"wikipedia", "query":"quantum entanglement", "at_unix":super::super::bench::now_unix(), "requests":count.load(std::sync::atomic::Ordering::Relaxed)});
        match answer {
            Ok(hits) => {
                receipt["status"] = "ok".into();
                receipt["hits"] = serde_json::to_value(hits).unwrap();
            }
            Err(VerticalError::Throttled(wait)) => {
                receipt["status"] = format!("rate-limited:{}", wait.as_secs()).into()
            }
            Err(_) => receipt["status"] = "failed".into(),
        }
        crate::config::write_private(
            std::path::Path::new(&path),
            &serde_json::to_vec_pretty(&receipt).unwrap(),
        )
        .unwrap();
    }

    fn secs(s: u64) -> Option<Duration> {
        Some(Duration::from_secs(s))
    }

    #[test]
    fn web_operators_do_not_become_api_search_terms() {
        let q = r#"intitle:"Attention Is All You Need" arxiv site:arxiv.org filetype:pdf"#;
        assert_eq!(api_query(q), "arxiv Attention Is All You Need");
        let url = endpoint("openalex", q).unwrap();
        let parsed = url::Url::parse(&url).unwrap();
        let search = parsed
            .query_pairs()
            .find(|(k, _)| k == "search")
            .unwrap()
            .1
            .into_owned();
        assert_eq!(search, "arxiv Attention Is All You Need");
        assert!(!endpoint("arxiv", q).unwrap().contains("ti:intitle"));
        assert_eq!(api_query("rust ownership"), "rust ownership");
        assert!(
            endpoint("github", "how to install nodejs with nvm on linux").is_none(),
            "classify the original words, not one URL-encoded token"
        );
        assert!(
            endpoint("github", "rust async runtime library")
                .unwrap()
                .contains("/search/repositories?")
        );
        assert!(
            endpoint("github", "python TypeError cannot read property")
                .unwrap()
                .contains("/search/issues?")
        );
    }

    #[test]
    fn stack_exchange_quota_is_a_throttle_with_its_own_wait() {
        let request_url = "https://api.stackexchange.com/2.3/search/advanced";
        let body = br#"{"error_id":502,"error_message":"too many requests from this IP, more requests available in 78310 seconds","error_name":"throttle_violation"}"#;
        // The origin's full published wait is binding, even beyond one hour.
        assert_eq!(throttle("stackexchange", 400, &[], body), secs(78310));
        assert_eq!(throttle("stackexchange", 200, &[], body), secs(78310));
        assert_eq!(
            stackexchange_wait(&serde_json::json!({"items":[],"backoff":120,"quota_remaining":42})),
            secs(120)
        );
        assert_eq!(
            stackexchange_wait(&serde_json::json!({"items":[],"backoff":0,"quota_remaining":42})),
            None
        );
        let exhausted =
            stackexchange_wait(&serde_json::json!({"items":[],"quota_remaining":0})).unwrap();
        assert_eq!(exhausted, Duration::from_secs(86_400));
        let data = stackexchange_data(r#"{"items":[
          {"title":"Nginx &quot;upstream closed&quot;", "link":"https://stackoverflow.com/questions/30470448/nginx", "score":22,"answer_count":2,"accepted_answer_id":7,
           "answers":[{"answer_id":8,"score":100,"body":"<p>This popular alternative must not replace the accepted answer.</p>"},
                      {"answer_id":7,"is_accepted":true,"score":8,"body":"<p>Point the app.sock under upstream django and django.sock under location /ws/.</p>"}]},
          {"title":"No accepted answer","link":"https://stackoverflow.com/questions/3/test","is_answered":true,"answer_count":1,
           "answers":[{"answer_id":9,"score":0,"body":"<p>Use proxy_read_timeout 300s and proxy_connect_timeout 75s.</p>"}]},
          {"title":"No usable answer","link":"https://stackoverflow.com/questions/4/test","is_answered":true,"answer_count":1,
           "answers":[{"answer_id":10,"score":-5,"body":"<p>An unaccepted, downvoted guess.</p>"}]},
          {"title":"Orion timeouts","link":"https://stackoverflow.com/questions/5/test","accepted_answer_id":11,"answer_count":1,
           "answers":[{"answer_id":11,"score":8,"body":"<pre><code>orion:\n    timeout: 42\n    retries: 3\n</code></pre>"}]}
        ],"quota_remaining":42}"#, request_url).unwrap();
        let hits = parse_stackexchange(&data, Some("nginx upstream header"));
        assert_eq!(hits.len(), 4);
        assert_eq!(hits[0].title, "Nginx \"upstream closed\"");
        assert!(hits[0].snippet.starts_with("Accepted answer:"));
        assert!(hits[0].snippet.contains("app.sock"));
        assert!(!hits[0].snippet.contains("popular alternative"));
        assert!(hits[1].snippet.starts_with("Answer:"));
        assert!(hits[1].snippet.contains("proxy_read_timeout"));
        assert!(
            !hits[2].snippet.contains("accepted answer"),
            "is_answered does not prove acceptance"
        );
        assert!(!hits[2].snippet.contains("downvoted guess"));
        let code = parse_stackexchange(&data, Some("how to set Orion timeout"));
        assert!(
            code[3].snippet.contains("\n    timeout: 42"),
            "{}",
            code[3].snippet
        );
        assert!(code[3].snippet.contains("\n    retries: 3"));
        assert!(
            hits.iter()
                .all(|h| h.snippet.chars().count() <= super::super::evidence::PASSAGE_CHARS)
        );
        for invalid in [
            "not JSON",
            r#"{"error_id":400,"items":[]}"#,
            r#"{"unexpected":[]}"#,
        ] {
            assert!(stackexchange_data(invalid, request_url).is_err());
        }
        assert!(stackexchange_data(r#"{"items":[]}"#, request_url).is_ok());
        assert!(
            endpoint("stackexchange", "nginx")
                .unwrap()
                .contains(STACKEXCHANGE_FILTER)
        );
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
            secs(5)
        );
        assert_eq!(
            throttle("openalex", 429, &h("retry-after", "86400"), b""),
            secs(86400)
        );
        let future =
            httpdate::fmt_http_date(std::time::SystemTime::now() + Duration::from_secs(7200));
        let wait = throttle("openalex", 503, &h("retry-after", &future), b"")
            .unwrap()
            .as_secs();
        assert!((7198..=7200).contains(&wait), "{wait}");
        assert_eq!(throttle("openalex", 503, &[], b""), None);
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
