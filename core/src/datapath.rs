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
    /// The interface answers this URL's question *better* than the document
    /// does, so it is asked first and the document is the fallback. True only
    /// where the document is a partial rendering of what the interface returns
    /// whole.
    pub preferred: bool,
    /// The interface's own next page, read from an answer too short to be
    /// content: asked once, its items appended. `None` where an answer is whole.
    pub next: Option<fn(&str, &str) -> Option<String>>,
}

pub static PATHS: &[DataPath] = &[
    DataPath {
        id: "reddit",
        hosts: &["reddit.com", "redd.it"],
        resolve: resolve_reddit,
        parse: parse_reddit_listing,
        min_items: 5,
        // No published rate limit: three calls in a row answered in full. The
        // encounter's own per-origin pacing is the one that applies.
        min_interval_ms: 0,
        // A subreddit's document server-renders three posts and leaves the rest
        // to JavaScript; this endpoint is what the page's own feed reloads with.
        preferred: true,
        // The popular feed's first page is the three posts the front page
        // server-renders; the rest follow its `load-after` cursor.
        next: Some(reddit_next_page),
    },
    DataPath {
        id: "news-feed",
        hosts: &["nytimes.com", "wsj.com"],
        resolve: resolve_news_feed,
        parse: parse_news_feed,
        min_items: 5,
        min_interval_ms: 0,
        preferred: false,
        next: None,
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
        preferred: false,
        next: None,
    },
    DataPath {
        id: "fourchan-catalog",
        hosts: &["4chan.org", "4channel.org"],
        resolve: resolve_fourchan,
        parse: parse_fourchan,
        min_items: 5,
        min_interval_ms: 0,
        preferred: false,
        next: None,
    },
    DataPath {
        id: "crates-api",
        hosts: &["crates.io"],
        resolve: resolve_crates,
        parse: parse_crates,
        min_items: 5,
        // crates.io's data-access policy: at most one request per second.
        min_interval_ms: 1_000,
        preferred: false,
        next: None,
    },
    DataPath {
        id: "twitch-gql",
        hosts: &["twitch.tv"],
        resolve: resolve_twitch,
        parse: parse_twitch,
        min_items: 3,
        min_interval_ms: 0,
        preferred: false,
        next: None,
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
    preferred: false,
    next: None,
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

/// Reddit's listing interface: the endpoint the page's own feed reloads with
/// (`<shreddit-feed reload-url=...>`) for a subreddit, and the popular feed the
/// front page's own document loads its posts from. One call is a whole page of
/// posts -- the same markup the page renders, every field on the `shreddit-post`
/// element -- it answers the plain client without a session, and it is not rate
/// limited the way the Atom feed is (three calls in a row, three full pages,
/// 2026-11-07). The document server-renders three posts and leaves the rest to
/// JavaScript, which is why this path is `preferred`.
fn resolve_reddit(url: &Url) -> Option<Request> {
    let mut target = match reddit_listing(url)? {
        Listing::Community(subreddit, sort) => format!(
            "https://www.reddit.com/svc/shreddit/community-more-posts/{sort}/?name={subreddit}"
        ),
        Listing::Popular(sort) => format!(
            "https://www.reddit.com/svc/shreddit/feeds/popular-feed?sort={}",
            sort.to_ascii_uppercase()
        ),
    };
    // A window the caller named (`?t=week`) is part of the question.
    if let Some((_, window)) = url.query_pairs().find(|(key, _)| key == "t") {
        if !window.is_empty() {
            target.push_str(&format!("&t={window}"));
        }
    }
    Some(Request {
        method: "GET",
        url: target,
        headers: vec![
            (
                "accept",
                "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
            ),
            ("sec-fetch-dest", "empty"),
            ("sec-fetch-mode", "cors"),
            ("sec-fetch-site", "same-origin"),
        ],
        body: None,
    })
}

fn is_popular(name: &str) -> bool {
    name.eq_ignore_ascii_case("popular") || name.eq_ignore_ascii_case("all")
}

enum Listing {
    Community(String, String),
    Popular(String),
}

/// Where a listing URL lives in the interface. `None` for everything that is
/// not a listing -- a thread is answered by its own document, and a service
/// path is nobody's listing.
fn reddit_listing(url: &Url) -> Option<Listing> {
    const SORTS: [&str; 6] = ["hot", "new", "top", "rising", "controversial", "best"];
    let segments: Vec<&str> = url.path().split('/').filter(|s| !s.is_empty()).collect();
    match segments.as_slice() {
        // The front page, signed out, is the popular feed (`?sort=BEST`).
        [] => Some(Listing::Popular("best".to_string())),
        [sort] if SORTS.contains(sort) => Some(Listing::Popular((*sort).to_string())),
        // Signed out, r/popular and r/all are the same feed: r/all's own
        // document loads the popular one (its `all-feed` asks for a human).
        ["r", feed] if is_popular(feed) => Some(Listing::Popular("best".to_string())),
        ["r", feed, sort] if is_popular(feed) && SORTS.contains(sort) => {
            Some(Listing::Popular((*sort).to_string()))
        }
        ["r", subreddit] => Some(Listing::Community(
            (*subreddit).to_string(),
            "hot".to_string(),
        )),
        ["r", subreddit, sort] if SORTS.contains(sort) => Some(Listing::Community(
            (*subreddit).to_string(),
            (*sort).to_string(),
        )),
        _ => None,
    }
}

/// The feed's own next page: the `load-after` partial it names, on the same
/// origin, only for a feed path.
fn reddit_next_page(body: &str, source: &str) -> Option<String> {
    let start = body.find("id=\"feed-next-page-partial\"")?;
    let tag_end = body[start..].find('>')? + start;
    let tag_start = body[..start].rfind('<')?;
    let tag = &body[tag_start..tag_end];
    let src = tag.split(" src=\"").nth(1)?.split('"').next()?;
    let src = src.replace("&amp;", "&");
    let base = Url::parse(source).ok()?;
    let next = base.join(&src).ok()?;
    (next.host_str() == base.host_str()
        && next.path().starts_with("/svc/shreddit/")
        && next
            .query_pairs()
            .any(|(key, value)| key == "after" && !value.is_empty()))
    .then(|| next.to_string())
}

/// The posts in a shreddit listing: title, permalink, and the line the reader
/// needs to choose one -- domain, score, author, comment count.
fn parse_reddit_listing(body: &str, source: &str) -> Vec<Item> {
    let document = scraper::Html::parse_document(body);
    let Ok(selector) = scraper::Selector::parse("shreddit-post") else {
        return Vec::new();
    };
    let root = Url::parse(source).ok();
    let mut items = Vec::new();
    for element in document.select(&selector) {
        let value = element.value();
        let title = value.attr("post-title").unwrap_or_default().trim();
        let link = value.attr("permalink").unwrap_or_default();
        if title.is_empty() || link.is_empty() {
            continue;
        }
        let url = root
            .as_ref()
            .and_then(|root| root.join(link).ok())
            .map(|url| url.to_string());
        let mut detail: Vec<String> = Vec::new();
        if value.attr("post-type") == Some("link") {
            if let Some(domain) = value.attr("domain").filter(|domain| !domain.is_empty()) {
                detail.push(format!("({domain})"));
            }
        }
        if let Some(score) = value.attr("score").filter(|score| !score.is_empty()) {
            detail.push(format!("{score} pts"));
        }
        if let Some(author) = value.attr("author").filter(|author| !author.is_empty()) {
            detail.push(format!("u/{author}"));
        }
        if let Some(comments) = value
            .attr("comment-count")
            .filter(|count| !count.is_empty() && *count != "0")
        {
            detail.push(format!("{comments} comments"));
        }
        items.push(Item {
            title: title.to_string(),
            detail: detail.join(" · "),
            url,
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

/// A front's feed, when it is alive: a feed whose newest dated story is more
/// than [`STALE_FEED_DAYS`] old has been abandoned, and its stories are not the
/// front's (WSJ's RSS stopped in January 2025). A stale feed answers nothing,
/// so the front is read as its own document.
fn parse_news_feed(body: &str, source: &str) -> Vec<Item> {
    let items = parse_rss_items(body, source);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs() as i64);
    if stale_feed(&items, now) {
        return Vec::new();
    }
    items
}

/// The age past which a front's newest story says the feed was abandoned.
const STALE_FEED_DAYS: i64 = 30;

/// Whether the newest dated item is older than [`STALE_FEED_DAYS`] at `now`
/// (Unix seconds). A feed with no dates is not judged by them.
fn stale_feed(items: &[Item], now: i64) -> bool {
    items
        .iter()
        .filter_map(|item| rss_day(&item.detail))
        .max()
        .is_some_and(|newest| now - newest > STALE_FEED_DAYS * 86_400)
}

/// An RSS `pubDate` (`Mon, 27 Jan 2025 14:23:00 -0500`) as the Unix second
/// its day began; the time and zone do not matter at a month's scale.
fn rss_day(value: &str) -> Option<i64> {
    let parts: Vec<&str> = value.split_whitespace().collect();
    let date = match parts.as_slice() {
        [weekday, day, month, year, ..] if weekday.ends_with(',') => (day, month, year),
        [day, month, year, ..] => (day, month, year),
        _ => return None,
    };
    let (day, month, year) = date;
    crate::cache::http_date(&format!("Thu, {day:0>2} {month} {year} 00:00:00 GMT"))
}

/// News fronts publish a feed of the stories the front page leads with. The
/// feed answers a front or a section; an article, or any other page, is
/// answered by its own document (a walled article stays a declared wall, never
/// a list of other stories).
fn resolve_news_feed(url: &Url) -> Option<Request> {
    let host = url.host_str()?;
    let segments: Vec<&str> = url
        .path()
        .split('/')
        .filter(|part| !part.is_empty())
        .collect();
    let feed = if host.ends_with("nytimes.com") {
        // Sections live at `/section/<name>`; a bare `/<name>` is the older
        // spelling of the same page.
        let section = match segments.as_slice() {
            [] => "",
            ["section", name] | [name] => name,
            _ => return None,
        };
        match section {
            "" => "https://rss.nytimes.com/services/xml/rss/nyt/HomePage.xml",
            "world" => "https://rss.nytimes.com/services/xml/rss/nyt/World.xml",
            "business" => "https://rss.nytimes.com/services/xml/rss/nyt/Business.xml",
            "technology" => "https://rss.nytimes.com/services/xml/rss/nyt/Technology.xml",
            "science" => "https://rss.nytimes.com/services/xml/rss/nyt/Science.xml",
            _ => return None,
        }
    } else if host.ends_with("wsj.com") {
        match segments.as_slice() {
            [] | ["news", "world"] => "https://feeds.a.dj.com/rss/RSSWorldNews.xml",
            _ => return None,
        }
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
    // The call below answers "what is being asked here": the site's front,
    // its question listing and a tag's listing. A question is answered by
    // its own document.
    let segments: Vec<&str> = url
        .path()
        .split('/')
        .filter(|part| !part.is_empty())
        .collect();
    let tagged = match segments.as_slice() {
        [] | ["questions"] => None,
        ["questions", "tagged", tags] => {
            // `rust+async` and `rust%20async` both mean two tags; the API
            // takes them `;`-separated. A literal `+` separates before
            // decoding, so `c%2B%2B` stays the tag `c++`.
            let tags: Vec<String> = tags
                .split('+')
                .flat_map(|part| {
                    percent_decode(part)
                        .split(' ')
                        .map(str::to_string)
                        .collect::<Vec<_>>()
                })
                .filter(|tag| !tag.is_empty())
                .collect();
            if tags.is_empty() {
                return None;
            }
            Some(tags.join(";"))
        }
        _ => return None,
    };
    // The API's `site` parameter is the network host without the TLD.
    let site = host.strip_prefix("www.").unwrap_or(host);
    let site = site.split('.').next().unwrap_or("stackoverflow");
    let site = if site == "stackoverflow" {
        "stackoverflow"
    } else {
        site
    };
    let mut target = Url::parse("https://api.stackexchange.com/2.3/questions").ok()?;
    target
        .query_pairs_mut()
        .append_pair("site", site)
        .append_pair("pagesize", "30")
        .append_pair("order", "desc")
        .append_pair("sort", "activity");
    if let Some(tags) = tagged {
        target.query_pairs_mut().append_pair("tagged", &tags);
    }
    target.query_pairs_mut().append_pair("filter", "default");
    let target = target.to_string();
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
    let segments: Vec<&str> = url
        .path()
        .split('/')
        .filter(|part| !part.is_empty())
        .collect();
    // The board list and a board's front page; a thread is answered by its
    // own document.
    let target = match segments.as_slice() {
        [] => "https://a.4cdn.org/boards.json".to_string(),
        [board] | [board, "catalog"] => format!("https://a.4cdn.org/{board}/1.json"),
        _ => return None,
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

/// A URL path segment with its `%XX` escapes decoded (lossily, as UTF-8).
fn percent_decode(segment: &str) -> String {
    let bytes = segment.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = |at: usize| bytes.get(at).and_then(|b| (*b as char).to_digit(16));
        match (bytes[i], hex(i + 1), hex(i + 2)) {
            (b'%', Some(hi), Some(lo)) => {
                out.push((hi * 16 + lo) as u8);
                i += 3;
            }
            (byte, _, _) => {
                out.push(byte);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
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
        // A markdown link, so every item is followable: the fetch tool turns
        // these into short handles when the caller asked for them, and the URL
        // stays visible when it did not.
        let title = item.title.replace('[', "\\[").replace(']', "\\]");
        match &item.url {
            Some(url) => out.push_str(&format!("- [{title}]({url})")),
            None => out.push_str(&format!("- {title}")),
        }
        if !item.detail.is_empty() {
            out.push_str(&format!(" — {}", item.detail));
        }
        out.push('\n');
    }
    out
}

/// The interface's own answer that the item does not exist: Stack Exchange
/// answers a deleted or never-asked question with `error_name: not_found` on a
/// non-200 status. A deleted question is a result, not a wall — the source that
/// owns the item said so, and falling through to the page's Cloudflare wall
/// would report a deletion as an identity gap.
pub fn absent(body: &str) -> bool {
    let lower = body.to_ascii_lowercase();
    lower.contains("\"error_name\":\"not_found\"") || lower.contains("\"error_name\": \"not_found\"")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_deleted_item_is_an_answer_not_a_wall() {
        // Stack Exchange's own words for a question that no longer exists.
        assert!(absent(
            r#"{"error_id":404,"error_message":"no questions found","error_name":"not_found"}"#
        ));
        assert!(absent(r#"{ "error_name": "not_found" }"#));
        // A normal answer (even an empty one) is not an absence.
        assert!(!absent(r#"{"items":[],"has_more":false}"#));
    }

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

    // An interface answers the question its URL asks. A feed of other
    // stories is no answer to an article, nor a question list to a question.
    // WSJ's world feed, still served in November 2026, last moved in
    // January 2025: its stories are not the front's.
    #[test]
    fn an_abandoned_feed_answers_nothing() {
        let item = |date: &str| Item {
            title: "A story".into(),
            detail: date.into(),
            url: None,
        };
        let now = crate::cache::http_date("Sun, 08 Nov 2026 10:00:00 GMT").unwrap();
        let wsj = [
            item("Mon, 27 Jan 2025 14:23:00 -0500"),
            item("Sun, 26 Jan 2025 09:00:00 -0500"),
        ];
        assert!(stale_feed(&wsj, now));
        let live = [
            item("Mon, 27 Jan 2025 14:23:00 -0500"),
            item("Sat, 07 Nov 2026 22:10:00 +0000"),
        ];
        assert!(!stale_feed(&live, now));
        // No dates, or dates in another shape: not judged by them.
        assert!(!stale_feed(&[item(""), item("yesterday")], now));
        assert_eq!(
            rss_day("7 Nov 2026 22:10:00 GMT"),
            rss_day("Sat, 07 Nov 2026 01:00:00 +0900")
        );
    }

    #[test]
    fn interfaces_answer_fronts_and_listings_and_leave_items_to_their_page() {
        let feed = |u: &str| plan(u).map(|(_, request)| request.url);
        assert_eq!(
            feed("https://www.nytimes.com/").as_deref(),
            Some("https://rss.nytimes.com/services/xml/rss/nyt/HomePage.xml")
        );
        assert_eq!(
            feed("https://www.nytimes.com/section/world").as_deref(),
            Some("https://rss.nytimes.com/services/xml/rss/nyt/World.xml")
        );
        assert_eq!(
            feed("https://www.nytimes.com/technology").as_deref(),
            Some("https://rss.nytimes.com/services/xml/rss/nyt/Technology.xml")
        );
        assert!(feed("https://www.nytimes.com/2026/01/01/us/politics/some-story.html").is_none());
        assert!(feed("https://www.nytimes.com/section/opinion").is_none());
        assert!(feed("https://www.nytimes.com/athletic/123/a-story/").is_none());
        assert!(feed("https://www.wsj.com/").is_some());
        assert!(feed("https://www.wsj.com/articles/a-story-12345").is_none());

        assert!(feed("https://stackoverflow.com/").is_some());
        assert!(feed("https://stackoverflow.com/questions").is_some());
        assert!(feed("https://stackoverflow.com/questions/11227809/why-is-it-faster").is_none());
        // A tag's listing asks the API for that tag's questions.
        let tagged = feed("https://stackoverflow.com/questions/tagged/c%2b%2b+async").unwrap();
        assert!(tagged.contains("tagged=c%2B%2B%3Basync"), "{tagged}");
        assert!(tagged.contains("site=stackoverflow"), "{tagged}");
        assert!(feed("https://stackoverflow.com/questions/tagged/").is_none());
        assert!(!feed("https://stackoverflow.com/questions")
            .unwrap()
            .contains("tagged"));
        assert!(feed("https://superuser.com/questions/42/title").is_none());

        assert_eq!(
            feed("https://boards.4chan.org/").as_deref(),
            Some("https://a.4cdn.org/boards.json")
        );
        assert_eq!(
            feed("https://boards.4chan.org/g/").as_deref(),
            Some("https://a.4cdn.org/g/1.json")
        );
        assert_eq!(
            feed("https://boards.4chan.org/g/catalog").as_deref(),
            Some("https://a.4cdn.org/g/1.json")
        );
        assert!(feed("https://boards.4chan.org/g/thread/12345678").is_none());
    }

    #[test]
    fn reddit_resolves_a_listing_to_its_own_feed_and_leaves_a_thread_to_its_page() {
        let (path, request) = plan("https://www.reddit.com/r/rust/").unwrap();
        assert_eq!(
            request.url,
            "https://www.reddit.com/svc/shreddit/community-more-posts/hot/?name=rust"
        );
        assert!(
            path.preferred,
            "the listing's document is the partial answer"
        );
        let (_, request) = plan("https://www.reddit.com/r/rust/top/?t=week").unwrap();
        assert_eq!(
            request.url,
            "https://www.reddit.com/svc/shreddit/community-more-posts/top/?name=rust&t=week"
        );
        // The front page, signed out, is the popular feed its own document loads.
        let (_, request) = plan("https://www.reddit.com/").unwrap();
        assert_eq!(
            request.url,
            "https://www.reddit.com/svc/shreddit/feeds/popular-feed?sort=BEST"
        );
        let (_, request) = plan("https://www.reddit.com/top/?t=day").unwrap();
        assert_eq!(
            request.url,
            "https://www.reddit.com/svc/shreddit/feeds/popular-feed?sort=TOP&t=day"
        );
        let (_, request) = plan("https://www.reddit.com/r/popular/new/").unwrap();
        assert_eq!(
            request.url,
            "https://www.reddit.com/svc/shreddit/feeds/popular-feed?sort=NEW"
        );
        let (_, request) = plan("https://www.reddit.com/r/all/").unwrap();
        assert_eq!(
            request.url,
            "https://www.reddit.com/svc/shreddit/feeds/popular-feed?sort=BEST"
        );
        // A bare path that is not a sort is nobody's listing.
        assert!(plan("https://www.reddit.com/settings/").is_none());
        // A thread is answered by its own document, not by an interface.
        assert!(plan("https://www.reddit.com/r/rust/comments/abc/title/").is_none());
        // A service path is nobody's listing.
        assert!(plan("https://www.reddit.com/svc/shreddit/community-more-posts/hot/").is_none());
        assert!(plan("https://www.reddit.com/r/rust/wiki/index").is_none());
    }

    #[test]
    fn the_popular_feed_names_its_own_next_page() {
        let source = "https://www.reddit.com/svc/shreddit/feeds/popular-feed?sort=BEST";
        let page = r#"<shreddit-post post-title="a" permalink="/r/a/comments/1/a/"></shreddit-post>
            <faceplate-partial slot="load-after" id="feed-next-page-partial" loading="programmatic"
              src="/svc/shreddit/feeds/popular-feed?after=dDNf%3D%3D&amp;distance=3&amp;sort=BEST" method="GET">"#;
        assert_eq!(
            reddit_next_page(page, source).as_deref(),
            Some("https://www.reddit.com/svc/shreddit/feeds/popular-feed?after=dDNf%3D%3D&distance=3&sort=BEST")
        );
        // Another origin, a page that is not the interface, or no cursor: no next page.
        let foreign = page.replace("src=\"/svc", "src=\"https://evil.example/svc");
        assert!(reddit_next_page(&foreign, source).is_none());
        let elsewhere = page.replace("/svc/shreddit/feeds/", "/settings/");
        assert!(reddit_next_page(&elsewhere, source).is_none());
        let no_cursor = page.replace("after=dDNf%3D%3D&amp;", "");
        assert!(reddit_next_page(&no_cursor, source).is_none());
        assert!(reddit_next_page("<shreddit-feed></shreddit-feed>", source).is_none());
    }

    #[test]
    fn a_shreddit_listing_becomes_posts_with_their_permalinks() {
        let listing = r#"<shreddit-feed><article data-post-id="t3_1xpb71b">
            <shreddit-post post-title="Servo Browser Engine Continues" post-type="link"
              permalink="/r/rust/comments/1xpb71b/servo_browser_engine/" domain="phoronix.com"
              score="137" author="HimikoTogaFromUSSR" comment-count="11"></shreddit-post>
            </article><article><shreddit-post post-title="Ask here" post-type="text"
              permalink="/r/rust/comments/1xls6as/ask_here/" domain="self.rust" score="0"
              author="nobody" comment-count="0"></shreddit-post></article></shreddit-feed>"#;
        let items = parse_reddit_listing(listing, "https://www.reddit.com/r/rust/");
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].title, "Servo Browser Engine Continues");
        assert_eq!(
            items[0].detail,
            "(phoronix.com) · 137 pts · u/HimikoTogaFromUSSR · 11 comments"
        );
        assert_eq!(
            items[0].url.as_deref(),
            Some("https://www.reddit.com/r/rust/comments/1xpb71b/servo_browser_engine/")
        );
        // A self post names no domain, and a zero comment count says nothing.
        assert_eq!(items[1].detail, "0 pts · u/nobody");
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
