//! Data paths: the app's own interface, not the document.
//!
//! A wall protects a *document*. Nearly every site that walls one also publishes
//! the same information through the interface its own web or mobile client calls:
//! a feed, a public API, a GraphQL endpoint. Those interfaces are not walls; they
//! are the product's front door, they answer in milliseconds, and a request to
//! them looks exactly like the app's own request because that is what it is.
//!
//! So the fetch order is: document → data path → borrowed lane. The data path is
//! not a bypass of a wall: it never touches the walled document. It asks the same
//! origin (or the origin's own API host) the question the app asks.
//!
//! The table is closed and inspectable, like every other contract in this design.
//! Each entry declares which URLs it serves, the request it makes, and how many
//! real items an answer must contain to count.

use serde::Serialize;
use url::Url;

#[derive(Clone, Debug, Serialize)]
pub struct Item {
    pub title: String,
    pub detail: String,
    pub url: Option<String>,
}

#[derive(Clone, Debug)]
pub struct Request {
    pub method: &'static str,
    pub url: String,
    /// Headers that make the request *the app's own*: an XHR, not a page load.
    pub headers: Vec<(&'static str, &'static str)>,
    pub body: Option<String>,
}

pub struct DataPath {
    pub id: &'static str,
    /// Host suffixes this path serves.
    pub hosts: &'static [&'static str],
    pub resolve: fn(&Url) -> Option<Request>,
    pub parse: fn(&str, &str) -> Vec<Item>,
    /// How many real items an answer must contain to be content.
    pub min_items: usize,
    /// Minimum spacing between two calls to this interface, in ms. Interfaces
    /// publish their own limits; going faster is how a client gets blocked.
    pub min_interval_ms: i64,
}

pub static PATHS: &[DataPath] = &[
    DataPath {
        id: "reddit-feed",
        hosts: &["reddit.com", "redd.it"],
        resolve: resolve_reddit,
        parse: parse_reddit_feed,
        min_items: 5,
        // Reddit's feed publishes x-ratelimit-reset≈9s on this IP.
        min_interval_ms: 11_000,
    },
    DataPath {
        id: "news-feed",
        hosts: &["nytimes.com", "wsj.com"],
        resolve: resolve_news_feed,
        parse: parse_rss_items,
        min_items: 5,
        min_interval_ms: 0,
    },
    DataPath {
        id: "stackexchange-api",
        hosts: &[
            "stackoverflow.com",
            "stackexchange.com",
            "superuser.com",
            "serverfault.com",
            "askubuntu.com",
            "mathoverflow.net",
        ],
        resolve: resolve_stackexchange,
        parse: parse_stackexchange,
        min_items: 5,
        min_interval_ms: 0,
    },
    DataPath {
        id: "fourchan-catalog",
        hosts: &["4chan.org", "4channel.org"],
        resolve: resolve_fourchan,
        parse: parse_fourchan,
        min_items: 5,
        min_interval_ms: 0,
    },
    DataPath {
        id: "crates-api",
        hosts: &["crates.io"],
        resolve: resolve_crates,
        parse: parse_crates,
        min_items: 5,
        // crates.io's data-access policy: at most one request per second.
        min_interval_ms: 1_000,
    },
    DataPath {
        id: "twitch-gql",
        hosts: &["twitch.tv"],
        resolve: resolve_twitch,
        parse: parse_twitch,
        min_items: 3,
        min_interval_ms: 0,
    },
];

/// A feed on a `.test` host, over plain HTTP so a proxy fixture can answer it.
#[cfg(test)]
static TEST_PATHS: &[DataPath] = &[DataPath {
    id: "test-feed",
    hosts: &["feed.test"],
    resolve: |url| {
        Some(Request {
            method: "GET",
            url: format!("http://{}/feed.rss", url.host_str()?),
            headers: Vec::new(),
            body: None,
        })
    },
    parse: parse_rss_items,
    min_items: 2,
    min_interval_ms: 0,
}];
#[cfg(not(test))]
static TEST_PATHS: &[DataPath] = &[];

pub fn find(host: &str) -> Option<&'static DataPath> {
    PATHS.iter().chain(TEST_PATHS).find(|path| {
        path.hosts
            .iter()
            .any(|suffix| host == *suffix || host.ends_with(&format!(".{suffix}")))
    })
}

/// Resolve the data request for a document URL, if this path serves it.
pub fn plan(url: &str) -> Option<(&'static DataPath, Request)> {
    let parsed = Url::parse(url).ok()?;
    let host = parsed.host_str()?;
    let path = find(host)?;
    let request = (path.resolve)(&parsed)?;
    Some((path, request))
}

// ---- reddit -------------------------------------------------------------

/// Reddit's `.json` is refused to non-session clients, its document is a shell,
/// and its Atom feed is open. The feed carries the same items the page shows.
fn resolve_reddit(url: &Url) -> Option<Request> {
    // Ask for the feed of the page that was requested, not of a different one.
    let path = url.path().trim_end_matches('/').to_string();
    // A thread is not a listing. Its feed carries the comments alone, without the
    // post the reader asked for, so it is not equivalent to the page: a thread is
    // answered by its own document (and the engine behind its gate). The feed
    // answers the listings it mirrors item for item.
    if path.contains("/comments/") {
        return None;
    }
    let target = format!("https://www.reddit.com{path}/.rss");
    Some(Request {
        method: "GET",
        url: target,
        headers: vec![
            (
                "accept",
                "application/atom+xml, application/xml;q=0.9, */*;q=0.8",
            ),
            ("sec-fetch-dest", "empty"),
            ("sec-fetch-mode", "cors"),
            ("sec-fetch-site", "same-origin"),
        ],
        body: None,
    })
}

fn parse_reddit_feed(body: &str, _source: &str) -> Vec<Item> {
    let mut items = Vec::new();
    for chunk in body.split("<entry>").skip(1) {
        let title = tag_text(chunk, "title").unwrap_or_default();
        let updated = tag_text(chunk, "updated").unwrap_or_default();
        let link = chunk
            .split("href=\"")
            .nth(1)
            .and_then(|rest| rest.split('"').next())
            .map(str::to_string);
        if title.is_empty() {
            continue;
        }
        items.push(Item {
            title: unescape(&title),
            detail: updated,
            url: link,
        });
    }
    items
}

/// RSS 2.0 items: title, link, pubDate.
fn parse_rss_items(body: &str, _source: &str) -> Vec<Item> {
    let mut items = Vec::new();
    for chunk in body.split("<item>").skip(1) {
        let title = tag_text(chunk, "title").unwrap_or_default();
        if title.is_empty() {
            continue;
        }
        let link = tag_text(chunk, "link").unwrap_or_default();
        let published = tag_text(chunk, "pubDate").unwrap_or_default();
        items.push(Item {
            title: unescape(&strip_cdata(&title)),
            detail: published,
            url: if link.is_empty() { None } else { Some(link) },
        });
    }
    items
}

/// News fronts publish a feed of the stories the front page leads with.
fn resolve_news_feed(url: &Url) -> Option<Request> {
    let host = url.host_str()?;
    let feed = if host.ends_with("nytimes.com") {
        // A section page maps to that section's feed.
        let section = url
            .path()
            .trim_matches('/')
            .split('/')
            .next()
            .unwrap_or("")
            .to_string();
        match section.as_str() {
            "world" => "https://rss.nytimes.com/services/xml/rss/nyt/World.xml",
            "business" => "https://rss.nytimes.com/services/xml/rss/nyt/Business.xml",
            "technology" => "https://rss.nytimes.com/services/xml/rss/nyt/Technology.xml",
            "science" => "https://rss.nytimes.com/services/xml/rss/nyt/Science.xml",
            _ => "https://rss.nytimes.com/services/xml/rss/nyt/HomePage.xml",
        }
    } else if host.ends_with("wsj.com") {
        "https://feeds.a.dj.com/rss/RSSWorldNews.xml"
    } else {
        return None;
    };
    Some(Request {
        method: "GET",
        url: feed.to_string(),
        headers: vec![
            (
                "accept",
                "application/rss+xml, application/xml;q=0.9, */*;q=0.8",
            ),
            ("sec-fetch-dest", "empty"),
            ("sec-fetch-mode", "cors"),
            ("sec-fetch-site", "same-site"),
        ],
        body: None,
    })
}

// ---- stack exchange ----------------------------------------------------

fn resolve_stackexchange(url: &Url) -> Option<Request> {
    let host = url.host_str()?;
    // The API host already answers JSON; it needs no rewrite.
    if host.starts_with("api.") {
        return None;
    }
    // The API's `site` parameter is the network host without the TLD.
    let site = host.strip_prefix("www.").unwrap_or(host);
    let site = site.split('.').next().unwrap_or("stackoverflow");
    let site = if site == "stackoverflow" {
        "stackoverflow"
    } else {
        site
    };
    let target = format!(
        "https://api.stackexchange.com/2.3/questions?site={site}&pagesize=30&order=desc&sort=activity&filter=default"
    );
    Some(Request {
        method: "GET",
        url: target,
        headers: vec![
            ("accept", "application/json"),
            ("sec-fetch-dest", "empty"),
            ("sec-fetch-mode", "cors"),
            ("sec-fetch-site", "cross-site"),
        ],
        body: None,
    })
}

fn parse_stackexchange(body: &str, _source: &str) -> Vec<Item> {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(body) else {
        return Vec::new();
    };
    let Some(items) = value["items"].as_array() else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|item| {
            let title = unescape(item["title"].as_str()?);
            if title.is_empty() {
                return None;
            }
            let score = item["score"].as_i64().unwrap_or(0);
            let answers = item["answer_count"].as_i64().unwrap_or(0);
            let tags = item["tags"]
                .as_array()
                .map(|tags| {
                    tags.iter()
                        .filter_map(|tag| tag.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                })
                .unwrap_or_default();
            Some(Item {
                title,
                detail: format!("score {score}, {answers} answers, {tags}"),
                url: item["link"].as_str().map(str::to_string),
            })
        })
        .collect()
}

// ---- crates.io ----------------------------------------------------------

/// crates.io's document is an Ember shell; the app reads its own JSON API on
/// the same origin: `/api/v1/summary` for the front page, `/api/v1/crates/<name>`
/// for a crate. Other hosts under the domain (index, static) are not the app.
fn resolve_crates(url: &Url) -> Option<Request> {
    if !matches!(url.host_str()?, "crates.io" | "www.crates.io") {
        return None;
    }
    let segments: Vec<&str> = url
        .path()
        .split('/')
        .filter(|part| !part.is_empty())
        .collect();
    let target = match segments.as_slice() {
        [] => "https://crates.io/api/v1/summary".to_string(),
        ["crates", name] | ["crates", name, ..] => {
            format!("https://crates.io/api/v1/crates/{name}")
        }
        _ => return None,
    };
    Some(Request {
        method: "GET",
        url: target,
        headers: vec![
            ("accept", "application/json"),
            ("sec-fetch-dest", "empty"),
            ("sec-fetch-mode", "cors"),
            ("sec-fetch-site", "same-origin"),
        ],
        body: None,
    })
}

fn parse_crates(body: &str, _source: &str) -> Vec<Item> {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(body) else {
        return Vec::new();
    };
    let crate_item = |entry: &serde_json::Value| -> Option<Item> {
        let name = entry["name"]
            .as_str()
            .or_else(|| entry["id"].as_str())?
            .to_string();
        let description = entry["description"]
            .as_str()
            .unwrap_or_default()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        let downloads = entry["downloads"].as_u64().unwrap_or(0);
        let version = entry["max_version"]
            .as_str()
            .or_else(|| entry["newest_version"].as_str())
            .unwrap_or_default();
        Some(Item {
            url: Some(format!("https://crates.io/crates/{name}")),
            title: name,
            detail: format!("{version}, {downloads} downloads — {description}")
                .trim_end_matches(" — ")
                .to_string(),
        })
    };
    let mut items: Vec<Item> = Vec::new();
    // A crate page: the crate, then its versions as the page lists them.
    if let Some(item) = value.get("crate").and_then(crate_item) {
        let name = item.title.clone();
        items.push(item);
        for version in value["versions"].as_array().into_iter().flatten() {
            let Some(num) = version["num"].as_str() else {
                continue;
            };
            items.push(Item {
                title: format!("{name} {num}"),
                detail: format!(
                    "{} downloads, {}",
                    version["downloads"].as_u64().unwrap_or(0),
                    version["created_at"].as_str().unwrap_or_default()
                ),
                url: Some(format!("https://crates.io/crates/{name}/{num}")),
            });
        }
        return items;
    }
    // The front page: its crate lists, in the order the page shows them.
    for list in [
        "new_crates",
        "most_downloaded",
        "just_updated",
        "most_recently_downloaded",
    ] {
        for entry in value[list].as_array().into_iter().flatten() {
            if let Some(item) = crate_item(entry) {
                if !items.iter().any(|seen| seen.title == item.title) {
                    items.push(item);
                }
            }
        }
    }
    items
}

// ---- 4chan -------------------------------------------------------------

fn resolve_fourchan(url: &Url) -> Option<Request> {
    let path = url.path().trim_matches('/');
    let target = match path.split('/').next() {
        Some(board) if !board.is_empty() => format!("https://a.4cdn.org/{board}/1.json"),
        _ => "https://a.4cdn.org/boards.json".to_string(),
    };
    Some(Request {
        method: "GET",
        url: target,
        headers: vec![
            ("accept", "application/json"),
            ("sec-fetch-dest", "empty"),
            ("sec-fetch-mode", "cors"),
            ("sec-fetch-site", "cross-site"),
        ],
        body: None,
    })
}

fn parse_fourchan(body: &str, source: &str) -> Vec<Item> {
    let board = source
        .trim_start_matches("https://a.4cdn.org/")
        .split('/')
        .next()
        .unwrap_or_default()
        .to_string();
    let Ok(value) = serde_json::from_str::<serde_json::Value>(body) else {
        return Vec::new();
    };
    let mut items = Vec::new();
    if let Some(boards) = value["boards"].as_array() {
        for board in boards {
            let title = board["title"].as_str().unwrap_or_default();
            let board_name = board["board"].as_str().unwrap_or_default();
            if title.is_empty() {
                continue;
            }
            items.push(Item {
                title: format!("/{board_name}/ {title}"),
                detail: board["meta_description"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string(),
                url: Some(format!("https://boards.4chan.org/{board_name}/")),
            });
        }
        return items;
    }
    if let Some(threads) = value["threads"].as_array() {
        for thread in threads {
            let Some(post) = thread["posts"].as_array().and_then(|posts| posts.first()) else {
                continue;
            };
            let subject = post["sub"].as_str().unwrap_or_default();
            let comment = strip_html(post["com"].as_str().unwrap_or_default());
            let title = if subject.is_empty() {
                comment.chars().take(90).collect::<String>()
            } else {
                subject.to_string()
            };
            if title.trim().is_empty() {
                continue;
            }
            items.push(Item {
                title: unescape(&title),
                detail: format!(
                    "{} replies, {} images",
                    post["replies"].as_i64().unwrap_or(0),
                    post["images"].as_i64().unwrap_or(0)
                ),
                url: Some(format!(
                    "https://boards.4chan.org/{board}/thread/{}",
                    post["no"].as_i64().unwrap_or(0)
                )),
            });
        }
    }
    items
}

// ---- twitch ------------------------------------------------------------

/// Twitch's public web client id, the one its own front page sends. The query is
/// the same shape the directory page requests; no integrity token is needed for
/// public directory data.
fn resolve_twitch(url: &Url) -> Option<Request> {
    if url.path() != "/" && !url.path().is_empty() {
        return None;
    }
    Some(Request {
        method: "POST",
        url: "https://gql.twitch.tv/gql".to_string(),
        headers: vec![
            ("accept", "*/*"),
            ("content-type", "application/json"),
            ("client-id", "kimne78kx3ncx6brgo4mv6wki5h1ko"),
            ("origin", "https://www.twitch.tv"),
            ("referer", "https://www.twitch.tv/"),
            ("sec-fetch-dest", "empty"),
            ("sec-fetch-mode", "cors"),
            ("sec-fetch-site", "same-site"),
        ],
        body: Some(
            r#"{"query":"{ streams(first: 20){ edges{ node{ title viewersCount broadcaster{ login displayName } } } } }"}"#
                .to_string(),
        ),
    })
}

fn parse_twitch(body: &str, _source: &str) -> Vec<Item> {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(body) else {
        return Vec::new();
    };
    let Some(edges) = value["data"]["streams"]["edges"].as_array() else {
        return Vec::new();
    };
    edges
        .iter()
        .filter_map(|edge| {
            let node = &edge["node"];
            let title = node["title"].as_str()?.to_string();
            let viewers = node["viewersCount"].as_i64().unwrap_or(0);
            let login = node["broadcaster"]["login"].as_str().unwrap_or_default();
            Some(Item {
                title,
                detail: format!("{viewers} viewers, {login}"),
                url: Some(format!("https://www.twitch.tv/{login}")),
            })
        })
        .collect()
}

// ---- helpers -----------------------------------------------------------

fn tag_text(chunk: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}");
    let start = chunk.find(&open)?;
    let after = chunk[start..].find('>')? + start + 1;
    let end = chunk[after..].find(&format!("</{tag}>"))? + after;
    Some(strip_cdata(&chunk[after..end]))
}

fn strip_cdata(value: &str) -> String {
    value
        .trim()
        .trim_start_matches("<![CDATA[")
        .trim_end_matches("]]>")
        .to_string()
}

fn strip_html(value: &str) -> String {
    let mut out = String::new();
    let mut inside = false;
    for character in value.chars() {
        match character {
            '<' => inside = true,
            '>' => inside = false,
            _ if !inside => out.push(character),
            _ => {}
        }
    }
    out
}

fn unescape(value: &str) -> String {
    value
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&apos;", "'")
}

/// Clean text for the caller: one line per item, nothing invented.
pub fn render(path: &DataPath, items: &[Item]) -> String {
    let mut out = format!("# {} ({} items)\n\n", path.id, items.len());
    for item in items {
        out.push_str(&format!("- {}", item.title));
        if !item.detail.is_empty() {
            out.push_str(&format!(" — {}", item.detail));
        }
        if let Some(url) = &item.url {
            out.push_str(&format!(" — {url}"));
        }
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_match_their_own_hosts_only() {
        assert!(find("www.reddit.com").is_some());
        assert!(find("stackoverflow.com").is_some());
        // The API host is already the data interface: no rewrite, no data path.
        assert!(plan("https://api.stackexchange.com/2.3/questions").is_none());
        assert!(find("notreddit.com").is_none());
        assert!(find("4chan.org").is_some());
        assert!(find("www.twitch.tv").is_some());
        assert!(find("crates.io").is_some());
    }

    #[test]
    fn reddit_resolves_a_listing_to_its_feed_and_leaves_a_thread_to_its_page() {
        let (_, request) = plan("https://www.reddit.com/").unwrap();
        assert_eq!(request.url, "https://www.reddit.com/.rss");
        let (_, request) = plan("https://www.reddit.com/r/rust/").unwrap();
        assert_eq!(request.url, "https://www.reddit.com/r/rust/.rss");
        // A thread's feed is its comments alone, without the post the reader
        // asked for: the document answers a thread, not the interface.
        assert!(plan("https://www.reddit.com/r/rust/comments/abc/title/").is_none());
    }

    #[test]
    fn stackexchange_targets_the_api_with_the_site_parameter() {
        let (_, request) = plan("https://stackoverflow.com/questions").unwrap();
        assert!(request.url.contains("api.stackexchange.com"));
        assert!(request.url.contains("site=stackoverflow"));
    }

    #[test]
    fn fourchan_picks_the_board_or_the_index() {
        let (_, request) = plan("https://www.4chan.org/").unwrap();
        assert!(request.url.ends_with("/boards.json"));
        let (_, request) = plan("https://boards.4chan.org/v/").unwrap();
        assert!(request.url.ends_with("/v/1.json"));
    }

    #[test]
    fn twitch_is_a_gql_post_with_the_public_client_id() {
        let (_, request) = plan("https://www.twitch.tv/").unwrap();
        assert_eq!(request.method, "POST");
        assert!(request.headers.iter().any(|(name, _)| *name == "client-id"));
        assert!(request.body.as_deref().unwrap().contains("streams"));
    }

    #[test]
    fn parsers_require_real_items() {
        let feed = "<feed><entry><title>A post</title><updated>2026-11-04</updated><link href=\"https://r/1\"/></entry></feed>";
        let items = parse_reddit_feed(feed, "https://www.reddit.com/r/popular/.rss");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].title, "A post");
        let se = r#"{"items":[{"title":"How do I X?","score":3,"answer_count":2,"tags":["rust"],"link":"https://so/1"}]}"#;
        let items = parse_stackexchange(se, "https://api.stackexchange.com/2.3/questions");
        assert_eq!(items[0].title, "How do I X?");
        assert!(items[0].detail.contains("score 3"));
        let bs = r#"{"boards":[{"board":"v","title":"Video Games","meta_description":"games"}]}"#;
        assert_eq!(
            parse_fourchan(bs, "https://a.4cdn.org/boards.json").len(),
            1
        );
        let gql = r#"{"data":{"streams":{"edges":[{"node":{"title":"Live","viewersCount":10,"broadcaster":{"login":"x"}}}]}}}"#;
        let items = parse_twitch(gql, "https://gql.twitch.tv/gql");
        assert_eq!(items[0].detail, "10 viewers, x");
    }

    #[test]
    fn crates_io_reads_the_apps_own_api_and_only_for_the_app_host() {
        let (_, request) = plan("https://crates.io/").unwrap();
        assert_eq!(request.url, "https://crates.io/api/v1/summary");
        let (_, request) = plan("https://crates.io/crates/serde/versions").unwrap();
        assert_eq!(request.url, "https://crates.io/api/v1/crates/serde");
        assert!(plan("https://index.crates.io/config.json").is_none());
        assert!(plan("https://crates.io/policies").is_none());
        let summary = r#"{"num_crates":3,"new_crates":[{"id":"a","name":"a","description":"First\n crate","max_version":"0.1.0","downloads":5}],"most_downloaded":[{"id":"a","name":"a"},{"id":"b","name":"b","downloads":9}]}"#;
        let items = parse_crates(summary, "https://crates.io/api/v1/summary");
        assert_eq!(
            items
                .iter()
                .map(|item| item.title.as_str())
                .collect::<Vec<_>>(),
            ["a", "b"]
        );
        assert_eq!(items[0].detail, "0.1.0, 5 downloads — First crate");
        assert_eq!(items[0].url.as_deref(), Some("https://crates.io/crates/a"));
        let page = r#"{"crate":{"id":"serde","name":"serde","max_version":"1.0.0"},"versions":[{"num":"1.0.0","downloads":7},{"num":"0.9.0"}]}"#;
        assert_eq!(parse_crates(page, "").len(), 3);
        assert!(parse_crates("<html>", "").is_empty());
    }

    #[test]
    fn unescape_handles_the_common_entities() {
        assert_eq!(unescape("a &amp; b &lt;c&gt;"), "a & b <c>");
    }
}
