//! YouTube adapter: a watch page as its video card (title, channel, date,
//! length, views, chapters and the full description), read from the player
//! response and initial data the page embeds for its own player.

use serde_json::Value;

use crate::extract::{ContentKind, ExtractOptions, Extracted};

pub fn extract(html: &str, url: &str, opts: &ExtractOptions) -> Option<Extracted> {
    let id = video_id(&url::Url::parse(url).ok()?)?;
    let player = embedded(html, "ytInitialPlayerResponse = ")?;
    let details = &player["videoDetails"];
    // The player must hold the video the URL names; anything else (a
    // consent page's stub, another video's state) is not this page.
    if details["videoId"].as_str()? != id {
        return None;
    }
    let title = details["title"].as_str()?.trim();
    if title.is_empty() {
        return None;
    }
    let micro = &player["microformat"]["playerMicroformatRenderer"];

    let author = details["author"]
        .as_str()
        .map(str::trim)
        .filter(|a| !a.is_empty())
        .map(str::to_string);
    let published = micro["publishDate"]
        .as_str()
        .or_else(|| micro["uploadDate"].as_str())
        .map(|d| d.get(..10).unwrap_or(d).to_string());
    let mut facts: Vec<String> = Vec::new();
    facts.extend(author.clone());
    facts.extend(published.clone());
    if micro["liveBroadcastDetails"]["isLiveNow"].as_bool() == Some(true) {
        facts.push("live now".to_string());
    } else if let Some(seconds) = details["lengthSeconds"]
        .as_str()
        .and_then(|s| s.parse::<u64>().ok())
        .filter(|s| *s > 0)
    {
        facts.push(duration(seconds));
    }
    if let Some(views) = details["viewCount"]
        .as_str()
        .and_then(|v| v.parse::<u64>().ok())
    {
        facts.push(format!("{} views", thousands(views)));
    }
    facts.extend(micro["category"].as_str().map(str::to_string));

    let mut md = format!("# {title}\n\n");
    let mut blocks = 1;
    if !facts.is_empty() {
        md.push_str(&facts.join(" · "));
        md.push_str("\n\n");
        blocks += 1;
    }
    let chapters = embedded(html, "ytInitialData = ")
        .map(|data| chapters(&data))
        .unwrap_or_default();
    if !chapters.is_empty() {
        md.push_str("## Chapters\n\n");
        for (at, name) in &chapters {
            md.push_str(&format!("- {at} {name}\n"));
        }
        md.push('\n');
        blocks += 1;
    }
    let description = details["shortDescription"].as_str().unwrap_or("").trim();
    if !description.is_empty() {
        md.push_str("## Description\n\n");
        for line in description.lines() {
            // A description line is text: one that reads as a markdown
            // heading would split the card's sections.
            if line.trim_start().starts_with("# ") || line.trim_start().starts_with("## ") {
                md.push('\\');
            }
            md.push_str(line.trim_end());
            md.push('\n');
        }
        blocks += 1;
    }
    let md = md.trim_end().to_string();

    let lang = crate::extract::language::detect_from_text(&md);
    let total = md.len();
    let max = opts.max_chars.unwrap_or(16_000).max(200);
    let (slice, next) = crate::extract::paginate_public(&md, opts.offset, max);
    Some(Extracted {
        page: None,
        tokens_est: slice.len() / 4,
        markdown: slice,
        title: Some(title.to_string()),
        byline: author,
        published,
        site: Some("YouTube".to_string()),
        total_chars: total,
        next_offset: next,
        blocks_total: blocks,
        blocks_shown: blocks,
        focus_omitted: false,
        thin: false,
        content_kind: ContentKind::Article,
        lang: lang.code,
        lang_source: None,
        script: Some(lang.script),
        quality: 0.9,
        pdf_pages: None,
        images: Vec::new(),
        fingerprint: None,
        via: Some("adapter:youtube"),
        partial: None,
    })
}

/// The video a watch URL names: `/watch?v=`, `/shorts/`, `/live/` or a
/// `youtu.be` link, as an 11-character video id.
fn video_id(u: &url::Url) -> Option<String> {
    let host = u.host_str()?;
    let host = host
        .strip_prefix("www.")
        .or_else(|| host.strip_prefix("m."))
        .unwrap_or(host);
    let segments: Vec<&str> = u.path_segments()?.filter(|s| !s.is_empty()).collect();
    let id = match (host, segments.as_slice()) {
        ("youtube.com" | "music.youtube.com", ["watch"]) => {
            u.query_pairs().find(|(k, _)| k == "v")?.1.into_owned()
        }
        ("youtube.com", ["shorts" | "live", id]) | ("youtu.be", [id]) => (*id).to_string(),
        _ => return None,
    };
    (id.len() == 11
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'))
    .then_some(id)
}

/// The first JSON object assigned after `marker` in the page's scripts.
fn embedded(html: &str, marker: &str) -> Option<Value> {
    html.match_indices(marker).take(4).find_map(|(at, _)| {
        let rest = &html[at + marker.len()..];
        let value = serde_json::Deserializer::from_str(rest)
            .into_iter::<Value>()
            .next()?
            .ok()?;
        value.is_object().then_some(value)
    })
}

/// Chapters as `(time, title)`: the creator's own, else the ones YouTube
/// generated, from the page's chapter panel.
fn chapters(data: &Value) -> Vec<(String, String)> {
    let Some(panels) = data["engagementPanels"].as_array() else {
        return Vec::new();
    };
    for wanted in ["description-chapters", "macro-markers"] {
        let mut out: Vec<(String, String)> = Vec::new();
        for panel in panels {
            let panel = &panel["engagementPanelSectionListRenderer"];
            if !panel["targetId"]
                .as_str()
                .is_some_and(|t| t.contains(wanted))
            {
                continue;
            }
            let Some(items) = panel["content"]["macroMarkersListRenderer"]["contents"].as_array()
            else {
                continue;
            };
            for item in items {
                let item = &item["macroMarkersListItemRenderer"];
                let (Some(name), Some(at)) = (text(&item["title"]), text(&item["timeDescription"]))
                else {
                    continue;
                };
                if !out.iter().any(|(a, n)| *a == at && *n == name) {
                    out.push((at, name));
                }
            }
        }
        if !out.is_empty() {
            return out;
        }
    }
    Vec::new()
}

/// A renderer's text: `simpleText`, or its `runs` joined.
fn text(value: &Value) -> Option<String> {
    let text = match value["simpleText"].as_str() {
        Some(simple) => simple.to_string(),
        None => value["runs"]
            .as_array()?
            .iter()
            .filter_map(|run| run["text"].as_str())
            .collect(),
    };
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_string())
}

fn duration(seconds: u64) -> String {
    let (h, m, s) = (seconds / 3600, seconds / 60 % 60, seconds % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

fn thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page(video: &str, description: &str) -> String {
        let player = serde_json::json!({
            "playabilityStatus": {"status": "OK"},
            "videoDetails": {
                "videoId": video, "title": "Building a compiler in a weekend",
                "lengthSeconds": "3725", "author": "Ada Lovelace",
                "viewCount": "1834934945", "shortDescription": description,
            },
            "microformat": {"playerMicroformatRenderer": {
                "publishDate": "2009-10-24T23:57:33-07:00", "category": "Education",
            }},
        });
        let data = serde_json::json!({"engagementPanels": [
            {"engagementPanelSectionListRenderer": {"targetId": "engagement-panel-comments-section"}},
            {"engagementPanelSectionListRenderer": {
                "targetId": "engagement-panel-macro-markers-description-chapters",
                "content": {"macroMarkersListRenderer": {"contents": [
                    {"macroMarkersListItemRenderer": {"title": {"simpleText": "Intro"}, "timeDescription": {"simpleText": "0:00"}}},
                    {"macroMarkersListItemRenderer": {"title": {"runs": [{"text": "The "}, {"text": "parser"}]}, "timeDescription": {"simpleText": "4:12"}}},
                ]}},
            }},
        ]});
        format!(
            "<html lang=\"ne-NP\"><title>YouTube</title><script>var ytInitialPlayerResponse = {player};var meta = document.createElement('meta');</script><script>var ytInitialData = {data};</script></html>"
        )
    }

    #[test]
    fn a_watch_page_reads_as_its_video_card() {
        let description =
            "Slides: https://example.com/slides\n\n# not a heading\nPart two follows next week.";
        let html = page("dQw4w9WgXcQ", description);
        let ex = extract(
            &html,
            "https://www.youtube.com/watch?v=dQw4w9WgXcQ&t=42s",
            &ExtractOptions::default(),
        )
        .unwrap();
        let md = &ex.markdown;
        assert!(
            md.starts_with("# Building a compiler in a weekend\n"),
            "{md}"
        );
        assert!(
            md.contains("Ada Lovelace · 2009-10-24 · 1:02:05 · 1,834,934,945 views · Education"),
            "{md}"
        );
        assert!(
            md.contains("## Chapters\n\n- 0:00 Intro\n- 4:12 The parser"),
            "{md}"
        );
        assert!(md.contains("Slides: https://example.com/slides"), "{md}");
        assert!(md.contains("\n\\# not a heading\n"), "{md}");
        assert_eq!(ex.byline.as_deref(), Some("Ada Lovelace"));
        assert_eq!(ex.published.as_deref(), Some("2009-10-24"));
        assert_eq!(ex.via, Some("adapter:youtube"));
        for url in [
            "https://youtu.be/dQw4w9WgXcQ",
            "https://m.youtube.com/watch?v=dQw4w9WgXcQ",
            "https://www.youtube.com/shorts/dQw4w9WgXcQ",
        ] {
            assert!(
                extract(&html, url, &ExtractOptions::default()).is_some(),
                "{url}"
            );
        }
    }

    #[test]
    fn only_the_video_the_url_names_is_read() {
        let html = page("dQw4w9WgXcQ", "A description.");
        for url in [
            // Another video's state, a channel, a search, a bad id.
            "https://www.youtube.com/watch?v=aaaaaaaaaaa",
            "https://www.youtube.com/@RickAstleyYT",
            "https://www.youtube.com/results?search_query=x",
            "https://www.youtube.com/watch?v=short",
            "https://notyoutube.com/watch?v=dQw4w9WgXcQ",
        ] {
            assert!(
                extract(&html, url, &ExtractOptions::default()).is_none(),
                "{url}"
            );
        }
        // No player state at all: the generic reading decides.
        assert!(
            extract(
                "<html><title>YouTube</title></html>",
                "https://www.youtube.com/watch?v=dQw4w9WgXcQ",
                &ExtractOptions::default()
            )
            .is_none()
        );
    }

    #[test]
    fn durations_and_counts_read_as_people_write_them() {
        assert_eq!(duration(213), "3:33");
        assert_eq!(duration(59), "0:59");
        assert_eq!(duration(3600), "1:00:00");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(1000), "1,000");
        assert_eq!(thousands(1_834_934_945), "1,834,934,945");
    }
}
