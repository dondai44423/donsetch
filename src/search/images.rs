//! Keyless image search (`web_search` with `intent: "image"`), contact-sheet
//! rendering, and direct raster image inspection (`web_fetch`).
//!
//! Queries Bing Images async HTML and the Wikimedia Commons MediaWiki API in
//! parallel using DonSeTch's trusted TLS roots and SSRF-safe redirect checks,
//! returns ranked `ImageHit` records with honest per-source status, and
//! optionally builds a rank-badged PNG contact sheet for vision-capable agents.

use std::collections::HashSet;
use std::io::Cursor;
use std::time::Duration;

use scraper::{Html, Selector};
use serde::{Deserialize, Serialize};
use serde_json::Value;

const USER_AGENT: &str = "Mozilla/5.0 (X11; Linux x86_64; rv:132.0) Gecko/20100101 Firefox/132.0";
const SOURCE_TIMEOUT: Duration = Duration::from_secs(8);
const THUMB_TIMEOUT: Duration = Duration::from_secs(6);
pub const MAX_DIRECT_IMAGE_BYTES: usize = 8 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageHit {
    pub title: String,
    pub image_url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thumb_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub page_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
    pub source: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageSourceReport {
    pub id: String,
    pub items: usize,
    pub status: String,
}

#[derive(Debug, Clone)]
pub struct ImageSearchOutcome {
    pub query: String,
    pub results: Vec<ImageHit>,
    pub sources: Vec<ImageSourceReport>,
    pub sheet_png: Option<Vec<u8>>,
    pub sheet_note: Option<String>,
}

fn is_http_url(u: &str) -> bool {
    let trimmed = u.trim();
    trimmed.starts_with("https://") || trimmed.starts_with("http://")
}

fn encode_query(q: &str) -> String {
    url::form_urlencoded::byte_serialize(q.as_bytes()).collect()
}

/// Parse Bing Images async HTML (`a.iusc[m]`) into structured hits.
pub fn parse_bing_images_html(body: &str, max: usize) -> Vec<ImageHit> {
    let doc = Html::parse_document(body);
    let Ok(sel) = Selector::parse("a.iusc") else {
        return Vec::new();
    };
    let mut hits = Vec::new();
    let mut seen = HashSet::new();
    for el in doc.select(&sel) {
        let Some(raw_m) = el.value().attr("m") else {
            continue;
        };
        let Ok(meta) = serde_json::from_str::<Value>(raw_m) else {
            continue;
        };
        let Some(murl) = meta.get("murl").and_then(Value::as_str).map(str::trim) else {
            continue;
        };
        if !is_http_url(murl) || !seen.insert(murl.to_string()) {
            continue;
        }
        let thumb = meta
            .get("turl")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| is_http_url(s))
            .map(str::to_string);
        let page = meta
            .get("purl")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| is_http_url(s))
            .map(str::to_string);
        let title = meta
            .get("t")
            .and_then(Value::as_str)
            .map(|s| s.split_whitespace().collect::<Vec<_>>().join(" "))
            .unwrap_or_default();
        hits.push(ImageHit {
            title,
            image_url: murl.to_string(),
            thumb_url: thumb,
            page_url: page,
            width: None,
            height: None,
            source: "bing-images".to_string(),
        });
        if hits.len() >= max {
            break;
        }
    }
    hits
}

/// Parse Wikimedia Commons `action=query&prop=imageinfo` JSON into structured hits.
pub fn parse_wikimedia_images_json(val: &Value, max: usize) -> Vec<ImageHit> {
    let Some(pages) = val
        .get("query")
        .and_then(|q| q.get("pages"))
        .and_then(Value::as_array)
    else {
        return Vec::new();
    };
    let mut hits = Vec::new();
    let mut seen = HashSet::new();
    for page in pages {
        let Some(info) = page
            .get("imageinfo")
            .and_then(Value::as_array)
            .and_then(|arr| arr.first())
        else {
            continue;
        };
        let Some(url) = info.get("url").and_then(Value::as_str).map(str::trim) else {
            continue;
        };
        if !is_http_url(url) || !seen.insert(url.to_string()) {
            continue;
        }
        let mime = info.get("mime").and_then(Value::as_str).unwrap_or("");
        if !mime.is_empty() && !mime.starts_with("image/") {
            continue;
        }
        let thumb = info
            .get("thumburl")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| is_http_url(s))
            .map(str::to_string);
        let page_url = info
            .get("descriptionurl")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| is_http_url(s))
            .map(str::to_string);
        let raw_title = page
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        let title = raw_title
            .strip_prefix("File:")
            .unwrap_or(raw_title)
            .replace('_', " ");
        let width = info.get("width").and_then(Value::as_u64).map(|n| n as u32);
        let height = info.get("height").and_then(Value::as_u64).map(|n| n as u32);
        hits.push(ImageHit {
            title,
            image_url: url.to_string(),
            thumb_url: thumb,
            page_url,
            width,
            height,
            source: "wikimedia".to_string(),
        });
        if hits.len() >= max {
            break;
        }
    }
    hits
}

/// Merge primary and secondary image hit lists, deduplicating by image URL.
pub fn merge_image_hits(
    primary: Vec<ImageHit>,
    secondary: Vec<ImageHit>,
    max: usize,
) -> Vec<ImageHit> {
    let mut out = Vec::with_capacity(max);
    let mut seen = HashSet::new();
    for hit in primary.into_iter().chain(secondary) {
        if seen.insert(hit.image_url.clone()) {
            out.push(hit);
            if out.len() >= max {
                break;
            }
        }
    }
    out
}

fn remaining_budget(cap: Duration) -> Duration {
    match super::SEARCH_DEADLINE.try_with(|d| *d).ok().flatten() {
        Some(deadline) => deadline
            .saturating_duration_since(tokio::time::Instant::now())
            .min(cap)
            .max(Duration::from_millis(250)),
        None => cap,
    }
}

fn build_http_client(timeout: Duration) -> Result<reqwest::Client, String> {
    let policy = reqwest::redirect::Policy::custom(|attempt| {
        if attempt.previous().len() >= 5 {
            attempt.error("too many redirects")
        } else if let Err(err) = crate::fetch::guards::validate_url_basic(attempt.url().as_str()) {
            attempt.error(err.to_string())
        } else {
            attempt.follow()
        }
    });
    crate::egress::trust::with_trusted_roots(reqwest::Client::builder())
        .user_agent(USER_AGENT)
        .timeout(timeout)
        .redirect(policy)
        .build()
        .map_err(|e| e.to_string())
}

async fn fetch_bing_images(
    client: &reqwest::Client,
    query: &str,
    max: usize,
) -> (Vec<ImageHit>, ImageSourceReport) {
    let count = (max * 2).clamp(12, 30);
    let url = format!(
        "https://www.bing.com/images/async?q={}&first=1&count={count}&mmasync=1&adlt=moderate",
        encode_query(query)
    );
    match client
        .get(&url)
        .header("Accept", "text/html,application/xhtml+xml")
        .header("Accept-Language", "en-US,en;q=0.9")
        .send()
        .await
    {
        Ok(resp) => {
            let status = resp.status();
            if !status.is_success() {
                return (
                    Vec::new(),
                    ImageSourceReport {
                        id: "bing-images".into(),
                        items: 0,
                        status: format!("http-{}", status.as_u16()),
                    },
                );
            }
            match resp.text().await {
                Ok(body) => {
                    let hits = parse_bing_images_html(&body, max * 2);
                    let st = if hits.is_empty() { "empty" } else { "ok" }.to_string();
                    let n = hits.len();
                    (
                        hits,
                        ImageSourceReport {
                            id: "bing-images".into(),
                            items: n,
                            status: st,
                        },
                    )
                }
                Err(_) => (
                    Vec::new(),
                    ImageSourceReport {
                        id: "bing-images".into(),
                        items: 0,
                        status: "read-error".into(),
                    },
                ),
            }
        }
        Err(e) => {
            let st = if e.is_timeout() { "timeout" } else { "error" };
            (
                Vec::new(),
                ImageSourceReport {
                    id: "bing-images".into(),
                    items: 0,
                    status: st.into(),
                },
            )
        }
    }
}

async fn fetch_wikimedia_images(
    client: &reqwest::Client,
    query: &str,
    max: usize,
) -> (Vec<ImageHit>, ImageSourceReport) {
    let gsrlimit = max.clamp(3, 12);
    let url = format!(
        "https://commons.wikimedia.org/w/api.php?action=query&generator=search&gsrsearch={}&gsrnamespace=6&gsrlimit={gsrlimit}&prop=imageinfo&iiprop=url%7Csize%7Cmime&iiurlwidth=640&format=json&formatversion=2",
        encode_query(query)
    );
    match client.get(&url).send().await {
        Ok(resp) => {
            let status = resp.status();
            if !status.is_success() {
                return (
                    Vec::new(),
                    ImageSourceReport {
                        id: "wikimedia".into(),
                        items: 0,
                        status: format!("http-{}", status.as_u16()),
                    },
                );
            }
            match resp.json::<Value>().await {
                Ok(val) => {
                    let hits = parse_wikimedia_images_json(&val, max);
                    let st = if hits.is_empty() { "empty" } else { "ok" }.to_string();
                    let n = hits.len();
                    (
                        hits,
                        ImageSourceReport {
                            id: "wikimedia".into(),
                            items: n,
                            status: st,
                        },
                    )
                }
                Err(_) => (
                    Vec::new(),
                    ImageSourceReport {
                        id: "wikimedia".into(),
                        items: 0,
                        status: "parse-error".into(),
                    },
                ),
            }
        }
        Err(e) => {
            let st = if e.is_timeout() { "timeout" } else { "error" };
            (
                Vec::new(),
                ImageSourceReport {
                    id: "wikimedia".into(),
                    items: 0,
                    status: st.into(),
                },
            )
        }
    }
}

/// Recognize direct raster image payloads (PNG/JPEG/WebP/GIF) by header or
/// magic bytes, returning the canonical MCP image MIME type and pixel
/// dimensions when decodable without full pixel expansion.
pub fn inspect_raster_image(
    body: &[u8],
    content_type: &str,
) -> Option<(&'static str, Option<(u32, u32)>)> {
    let header_mime = content_type
        .split(';')
        .next()
        .map(str::trim)
        .unwrap_or("")
        .to_ascii_lowercase();

    let magic_mime = if body.starts_with(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]) {
        Some("image/png")
    } else if body.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("image/jpeg")
    } else if body.len() >= 12 && &body[0..4] == b"RIFF" && &body[8..12] == b"WEBP" {
        Some("image/webp")
    } else if body.starts_with(b"GIF87a") || body.starts_with(b"GIF89a") {
        Some("image/gif")
    } else {
        None
    };

    let mime = match magic_mime {
        Some(m) => m,
        None => match header_mime.as_str() {
            "image/png" => "image/png",
            "image/jpeg" | "image/jpg" => "image/jpeg",
            "image/webp" => "image/webp",
            "image/gif" => "image/gif",
            _ => return None,
        },
    };

    let dims = image::ImageReader::new(Cursor::new(body))
        .with_guessed_format()
        .ok()
        .and_then(|r| r.into_dimensions().ok());

    Some((mime, dims))
}

/// Tiny 3x5 bitmap glyphs for `#` and `0`-`9` (zero external font deps).
fn glyph_3x5(ch: char) -> [u8; 5] {
    match ch {
        '#' => [0b010, 0b111, 0b010, 0b111, 0b010],
        '0' => [0b111, 0b101, 0b101, 0b101, 0b111],
        '1' => [0b010, 0b110, 0b010, 0b010, 0b111],
        '2' => [0b111, 0b001, 0b111, 0b100, 0b111],
        '3' => [0b111, 0b001, 0b111, 0b001, 0b111],
        '4' => [0b101, 0b101, 0b111, 0b001, 0b001],
        '5' => [0b111, 0b100, 0b111, 0b001, 0b111],
        '6' => [0b111, 0b100, 0b111, 0b101, 0b111],
        '7' => [0b111, 0b001, 0b001, 0b010, 0b010],
        '8' => [0b111, 0b101, 0b111, 0b101, 0b111],
        '9' => [0b111, 0b101, 0b111, 0b001, 0b111],
        _ => [0; 5],
    }
}

/// Stamp a high-contrast `#N` rank badge at `(x0, y0)` on a contact-sheet tile.
fn draw_rank_badge(canvas: &mut image::RgbaImage, x0: u32, y0: u32, rank: usize) {
    use image::Rgba;

    let label = format!("#{rank}");
    let scale: u32 = 3;
    let char_w = 3 * scale;
    let char_h = 5 * scale;
    let gap = scale;
    let pad_x: u32 = 6;
    let pad_y: u32 = 5;
    let n = label.chars().count() as u32;
    let box_w = pad_x * 2 + n * char_w + n.saturating_sub(1) * gap;
    let box_h = pad_y * 2 + char_h;

    let bx = x0 + 6;
    let by = y0 + 6;
    let bg = Rgba([10, 12, 16, 255]);
    let border = Rgba([225, 65, 55, 255]);
    let fg = Rgba([245, 247, 250, 255]);

    for y in by..(by + box_h).min(canvas.height()) {
        for x in bx..(bx + box_w).min(canvas.width()) {
            let is_edge = x == bx || x + 1 == bx + box_w || y == by || y + 1 == by + box_h;
            canvas.put_pixel(x, y, if is_edge { border } else { bg });
        }
    }

    for (ci, ch) in label.chars().enumerate() {
        let rows = glyph_3x5(ch);
        let gx = bx + pad_x + (ci as u32) * (char_w + gap);
        let gy = by + pad_y;
        for (ry, bits) in rows.iter().enumerate() {
            for rx in 0..3u32 {
                if (bits >> (2 - rx)) & 1 == 1 {
                    for sy in 0..scale {
                        for sx in 0..scale {
                            let px = gx + rx * scale + sx;
                            let py = gy + (ry as u32) * scale + sy;
                            if px < canvas.width() && py < canvas.height() {
                                canvas.put_pixel(px, py, fg);
                            }
                        }
                    }
                }
            }
        }
    }
}

/// Composite up to 9 rank-tagged images into a dark 3-column PNG contact sheet.
/// Each tile carries a `#N` badge matching its 1-based rank in the result list.
pub fn build_contact_sheet_png(images: &[(usize, image::DynamicImage)]) -> Result<Vec<u8>, String> {
    use image::{GenericImage, ImageFormat, Rgba, RgbaImage, imageops::FilterType};

    if images.is_empty() {
        return Err("no images decoded for contact sheet".into());
    }
    let cols: u32 = images.len().min(3) as u32;
    let rows: u32 = (images.len() as u32).div_ceil(cols);
    let cell_w: u32 = 420;
    let cell_h: u32 = 280;
    let pad: u32 = 10;

    let sheet_w = cols * cell_w + (cols + 1) * pad;
    let sheet_h = rows * cell_h + (rows + 1) * pad;
    let mut canvas = RgbaImage::from_pixel(sheet_w, sheet_h, Rgba([12, 13, 18, 255]));

    for (slot, (rank, img)) in images.iter().enumerate() {
        let r = (slot as u32) / cols;
        let c = (slot as u32) % cols;
        let x0 = pad + c * (cell_w + pad);
        let y0 = pad + r * (cell_h + pad);

        // Cell backdrop
        for y in y0..(y0 + cell_h) {
            for x in x0..(x0 + cell_w) {
                canvas.put_pixel(x, y, Rgba([22, 24, 32, 255]));
            }
        }
        let thumb = img.resize(cell_w, cell_h, FilterType::Triangle).to_rgba8();
        let ox = x0 + (cell_w.saturating_sub(thumb.width())) / 2;
        let oy = y0 + (cell_h.saturating_sub(thumb.height())) / 2;
        let _ = canvas.copy_from(&thumb, ox, oy);
        draw_rank_badge(&mut canvas, x0, y0, *rank);
    }

    let mut out = Cursor::new(Vec::new());
    image::DynamicImage::ImageRgba8(canvas)
        .write_to(&mut out, ImageFormat::Png)
        .map_err(|e| e.to_string())?;
    Ok(out.into_inner())
}

async fn fetch_contact_sheet(
    client: &reqwest::Client,
    hits: &[ImageHit],
) -> (Option<Vec<u8>>, Option<String>) {
    use futures_util::future::join_all;

    let futs = hits.iter().take(9).enumerate().map(|(idx, hit)| {
        let rank = idx + 1;
        let client = client.clone();
        let primary = hit
            .thumb_url
            .clone()
            .unwrap_or_else(|| hit.image_url.clone());
        let fallback = hit.image_url.clone();
        async move {
            for candidate in [primary, fallback] {
                if crate::fetch::guards::ensure_url_safe(&candidate)
                    .await
                    .is_err()
                {
                    continue;
                }
                let Ok(resp) = client.get(&candidate).timeout(THUMB_TIMEOUT).send().await else {
                    continue;
                };
                if !resp.status().is_success() {
                    continue;
                }
                let Ok(bytes) = resp.bytes().await else {
                    continue;
                };
                if bytes.len() > MAX_DIRECT_IMAGE_BYTES {
                    continue;
                }
                if let Ok(img) = image::load_from_memory(&bytes)
                    && img.width() >= 32
                    && img.height() >= 32
                {
                    return Some((rank, img));
                }
            }
            None
        }
    });
    let decoded: Vec<(usize, image::DynamicImage)> =
        join_all(futs).await.into_iter().flatten().collect();
    if decoded.is_empty() {
        return (
            None,
            Some("contact_sheet requested, but no thumbnails could be fetched cleanly".into()),
        );
    }
    match build_contact_sheet_png(&decoded) {
        Ok(png) => (Some(png), None),
        Err(e) => (None, Some(format!("contact_sheet encoding failed: {e}"))),
    }
}

/// Run a keyless image search across Bing Images + Wikimedia Commons.
pub async fn search_images(query: &str, max: usize, contact_sheet: bool) -> ImageSearchOutcome {
    let max = max.clamp(1, 12);
    let timeout = remaining_budget(SOURCE_TIMEOUT);
    let client = match build_http_client(timeout) {
        Ok(c) => c,
        Err(e) => {
            return ImageSearchOutcome {
                query: query.to_string(),
                results: Vec::new(),
                sources: vec![ImageSourceReport {
                    id: "http".into(),
                    items: 0,
                    status: format!("client-error: {e}"),
                }],
                sheet_png: None,
                sheet_note: None,
            };
        }
    };

    let ((bing_hits, bing_rep), (wiki_hits, wiki_rep)) = tokio::join!(
        fetch_bing_images(&client, query, max),
        fetch_wikimedia_images(&client, query, max),
    );
    let results = merge_image_hits(bing_hits, wiki_hits, max);
    let sources = vec![bing_rep, wiki_rep];

    let (sheet_png, sheet_note) = if contact_sheet && !results.is_empty() {
        fetch_contact_sheet(&client, &results).await
    } else {
        (None, None)
    };

    ImageSearchOutcome {
        query: query.to_string(),
        results,
        sources,
        sheet_png,
        sheet_note,
    }
}

/// Render compact Markdown for image search results matching DonSeTch's output shape.
pub fn render_image_markdown(out: &ImageSearchOutcome) -> String {
    let mut lines = vec!["# Image search results".to_string()];
    for (idx, hit) in out.results.iter().enumerate() {
        let title = if hit.title.is_empty() {
            "(untitled image)"
        } else {
            hit.title.as_str()
        };
        let dims = match (hit.width, hit.height) {
            (Some(w), Some(h)) => format!(" · {w}x{h}"),
            _ => String::new(),
        };
        lines.push(format!(
            "{}. {} · {}{} [{}]",
            idx + 1,
            hit.image_url,
            title,
            dims,
            hit.source
        ));
        if let Some(page) = &hit.page_url {
            lines.push(format!("   source: {page}"));
        }
    }
    if let Some(note) = &out.sheet_note {
        lines.push(format!("note: {note}"));
    }
    let census = out
        .sources
        .iter()
        .map(|s| format!("{} {} ({})", s.id, s.items, s.status))
        .collect::<Vec<_>>()
        .join(" · ");
    lines.push(format!("sources: {census}"));
    format!("{}\n", lines.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn bing_parser_extracts_valid_hits_and_drops_non_http_and_dupes() {
        let html = r#"
        <html><body>
          <a class="iusc" m='{"murl":"https://img.example.com/a.jpg","turl":"https://t.example.com/a.jpg","purl":"https://example.com/page-a","t":"Nine-Ball   Unit"}'></a>
          <a class="iusc" m='{"murl":"https://img.example.com/a.jpg","t":"Duplicate"}'></a>
          <a class="iusc" m='{"murl":"javascript:alert(1)","t":"Bad Scheme"}'></a>
          <a class="iusc" m='not-json'></a>
          <a class="iusc" m='{"murl":"http://img.example.com/b.png","purl":"https://example.com/page-b","t":"Garage HUD"}'></a>
        </body></html>
        "#;
        let hits = parse_bing_images_html(html, 10);
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].title, "Nine-Ball Unit");
        assert_eq!(hits[0].image_url, "https://img.example.com/a.jpg");
        assert_eq!(
            hits[0].thumb_url.as_deref(),
            Some("https://t.example.com/a.jpg")
        );
        assert_eq!(
            hits[0].page_url.as_deref(),
            Some("https://example.com/page-a")
        );
        assert_eq!(hits[1].image_url, "http://img.example.com/b.png");
    }

    #[test]
    fn wikimedia_parser_extracts_imageinfo_and_skips_non_images() {
        let payload = json!({
            "query": {
                "pages": [
                    {
                        "title": "File:Mecha_diagram.png",
                        "imageinfo": [{
                            "url": "https://upload.wikimedia.org/mecha.png",
                            "thumburl": "https://upload.wikimedia.org/thumb/mecha.png",
                            "descriptionurl": "https://commons.wikimedia.org/wiki/File:Mecha_diagram.png",
                            "width": 1200,
                            "height": 800,
                            "mime": "image/png"
                        }]
                    },
                    {
                        "title": "File:Audio_clip.ogg",
                        "imageinfo": [{
                            "url": "https://upload.wikimedia.org/clip.ogg",
                            "mime": "audio/ogg"
                        }]
                    },
                    {
                        "title": "File:Missing_info.jpg"
                    }
                ]
            }
        });
        let hits = parse_wikimedia_images_json(&payload, 5);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].title, "Mecha diagram.png");
        assert_eq!(hits[0].width, Some(1200));
        assert_eq!(hits[0].height, Some(800));
        assert_eq!(hits[0].source, "wikimedia");
    }

    #[test]
    fn merge_deduplicates_across_sources_and_respects_limit() {
        let mk = |u: &str, src: &str| ImageHit {
            title: u.into(),
            image_url: u.into(),
            thumb_url: None,
            page_url: None,
            width: None,
            height: None,
            source: src.into(),
        };
        let merged = merge_image_hits(
            vec![
                mk("https://a.test/1.jpg", "bing"),
                mk("https://a.test/2.jpg", "bing"),
            ],
            vec![
                mk("https://a.test/2.jpg", "wikimedia"),
                mk("https://a.test/3.jpg", "wikimedia"),
            ],
            2,
        );
        assert_eq!(merged.len(), 2);
        assert_eq!(merged[0].image_url, "https://a.test/1.jpg");
        assert_eq!(merged[1].image_url, "https://a.test/2.jpg");
    }

    #[test]
    fn contact_sheet_encodes_valid_png_with_rank_badges() {
        let a = image::DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(
            64,
            48,
            image::Rgba([200, 30, 40, 255]),
        ));
        let b = image::DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(
            48,
            64,
            image::Rgba([40, 220, 90, 255]),
        ));
        // Non-contiguous ranks (1 and 3) simulate thumbnail #2 failing while
        // preserving exact list numbering on the contact sheet.
        let png = build_contact_sheet_png(&[(1, a), (3, b)]).expect("valid contact sheet png");
        assert!(png.starts_with(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]));
        let decoded = image::load_from_memory(&png)
            .expect("png decodes")
            .to_rgba8();
        assert!(decoded.width() > 400 && decoded.height() > 200);
        // Badge border at tile 0 top-left (pad=10, offset=6 -> x=16, y=16).
        assert_eq!(decoded.get_pixel(16, 16), &image::Rgba([225, 65, 55, 255]));
    }

    #[test]
    fn inspect_raster_image_identifies_images_and_rejects_archives() {
        let img = image::DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(
            40,
            24,
            image::Rgba([10, 20, 30, 255]),
        ));
        let mut png = Cursor::new(Vec::new());
        img.write_to(&mut png, image::ImageFormat::Png).unwrap();
        let (mime, dims) =
            inspect_raster_image(png.get_ref(), "application/octet-stream").expect("png detected");
        assert_eq!(mime, "image/png");
        assert_eq!(dims, Some((40, 24)));

        let (webp_mime, _) =
            inspect_raster_image(b"RIFF\x00\x00\x00\x00WEBPxxxx", "image/webp").expect("webp");
        assert_eq!(webp_mime, "image/webp");

        assert!(inspect_raster_image(b"PK\x03\x04zipbytes", "application/zip").is_none());
        assert!(inspect_raster_image(b"<html>hi</html>", "text/html").is_none());
    }
}
