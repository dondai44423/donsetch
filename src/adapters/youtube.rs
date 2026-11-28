//! YouTube adapter: ultra-fast, cross-platform video cards and on-demand
//! timestamped transcripts via YouTube's public Innertube player endpoint,
//! with automatic watch-page HTML fallback.
//!
//! Token discipline:
//! - Default fetch (`transcript` omitted / `false`) returns only the compact
//!   video card (title, channel, date, duration, views, likes, category,
//!   chapters, full description) plus a one-line availability hint when
//!   captions exist — zero transcript tokens are spent unless requested.
//! - Opt-in fetch (`transcript: true` or `transcript: "<lang>"`) downloads
//!   only the selected compact caption track (`srv1` XML, ~3–10× smaller than
//!   `srv3`/`json3`), groups cues into readable timestamped paragraphs aligned
//!   with chapters, and appends `## Transcript` before `extract::view::render`
//!   so `focus`, `must_contain`, `toc`, `section`, and `offset` all work over
//!   the transcript.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use reqwest::Client as HttpClient;
use serde_json::{Value, json};

use crate::egress::proxy::{Proxy, ProxyScheme};
use crate::extract::{ContentKind, ExtractOptions, Extracted};

const INNERTUBE_PLAYER: &str = "https://www.youtube.com/youtubei/v1/player?prettyPrint=false";
const IOS_CLIENT_VERSION: &str = "20.10.4";
const IOS_USER_AGENT: &str =
    "com.google.ios.youtube/20.10.4 (iPhone16,2; U; CPU iOS 18_3_2 like Mac OS X;)";
const ANDROID_CLIENT_VERSION: &str = "20.10.38";
const ANDROID_USER_AGENT: &str = "com.google.android.youtube/20.10.38 (Linux; U; Android 11) gzip";
const WEB_CLIENT_VERSION: &str = "2.20260301.00.00";
const WEB_USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/134.0.0.0 Safari/537.36";

const MAX_PLAYER_BYTES: usize = 4 * 1024 * 1024;
const MAX_CAPTION_BYTES: usize = 8 * 1024 * 1024;
const CACHE_TTL: Duration = Duration::from_secs(600);
const VIDEO_CACHE_MAX: usize = 64;
const TRANSCRIPT_CACHE_MAX: usize = 64;
const MAX_META_TRACKS: usize = 12;

/// Caller's opt-in transcript preference for video URLs.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum TranscriptRequest {
    #[default]
    Off,
    Default,
    Lang(String),
}

impl TranscriptRequest {
    pub fn is_requested(&self) -> bool {
        !matches!(self, Self::Off)
    }
}

/// Parse the MCP/CLI `transcript` argument (`false`/absent, `true`, or BCP-47 language code).
pub fn parse_transcript_arg(value: Option<&Value>) -> Result<TranscriptRequest, String> {
    match value {
        None | Some(Value::Null) | Some(Value::Bool(false)) => Ok(TranscriptRequest::Off),
        Some(Value::Bool(true)) => Ok(TranscriptRequest::Default),
        Some(Value::String(raw)) => {
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                return Err("transcript language code must not be empty".to_string());
            }
            if trimmed.eq_ignore_ascii_case("true") {
                return Ok(TranscriptRequest::Default);
            }
            if trimmed.eq_ignore_ascii_case("false") {
                return Ok(TranscriptRequest::Off);
            }
            if trimmed.len() > 24
                || !trimmed
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
            {
                return Err(format!(
                    "transcript must be true, false, or a language code like \"en\" or \"pt-BR\", got {trimmed:?}"
                ));
            }
            Ok(TranscriptRequest::Lang(trimmed.replace('_', "-")))
        }
        Some(other) => Err(format!(
            "transcript must be a boolean or language code string, got {other}"
        )),
    }
}

#[derive(Clone, Debug)]
struct CaptionTrack {
    lang: String,
    name: String,
    base_url: String,
    auto_generated: bool,
    translatable: bool,
}

#[derive(Clone, Debug)]
struct VideoRecord {
    id: String,
    title: String,
    author: Option<String>,
    published: Option<String>,
    length_seconds: Option<u64>,
    is_live: bool,
    views: Option<u64>,
    likes: Option<u64>,
    category: Option<String>,
    is_unlisted: bool,
    description: String,
    chapters: Vec<(String, String)>,
    tracks: Vec<CaptionTrack>,
    client_ua: &'static str,
}

#[derive(Clone, Debug)]
struct CachedTranscript {
    markdown: String,
    lang: String,
    auto_generated: bool,
    translated: bool,
    cues: usize,
    cached_at: Instant,
}

struct CachedVideo {
    record: VideoRecord,
    cached_at: Instant,
}

/// Successful live Innertube acquisition ready for the shared view/history pipeline.
pub struct LiveVideo {
    pub extracted: Extracted,
    pub transcript_meta: Option<Value>,
    pub available_tracks: Vec<String>,
    pub cached: bool,
    pub elapsed_ms: u128,
}

/// Outcome of the live YouTube Innertube fast-path.
pub enum LiveOutcome {
    Video(Box<LiveVideo>),
    Unavailable {
        status: u16,
        reason: String,
        auth_required: bool,
    },
    Fallback,
}

static HTTP_CLIENTS: LazyLock<Mutex<HashMap<String, HttpClient>>> = LazyLock::new(Default::default);
static VIDEO_CACHE: LazyLock<Mutex<HashMap<String, CachedVideo>>> = LazyLock::new(Default::default);
static TRANSCRIPT_CACHE: LazyLock<Mutex<HashMap<(String, String), CachedTranscript>>> =
    LazyLock::new(Default::default);

fn http_client(proxy: Option<&Proxy>) -> Result<HttpClient, String> {
    let route_key = proxy
        .map(Proxy::connection_key)
        .unwrap_or_else(|| "direct".into());
    if let Some(existing) = HTTP_CLIENTS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(&route_key)
        .cloned()
    {
        return Ok(existing);
    }
    let mut builder = crate::egress::trust::with_trusted_roots(HttpClient::builder())
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(6))
        .pool_max_idle_per_host(4)
        .no_proxy();
    if let Some(proxy) = proxy {
        if proxy.scheme == ProxyScheme::Socks5 {
            return Err("socks5 proxy not supported by reqwest youtube client".into());
        }
        builder = builder
            .proxy(reqwest::Proxy::all(proxy.wire_url()).map_err(|e| format!("proxy: {e}"))?);
    }
    let built = builder.build().map_err(|e| format!("http client: {e}"))?;
    let mut map = HTTP_CLIENTS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if map.len() >= 16 {
        map.clear();
    }
    map.insert(route_key, built.clone());
    Ok(built)
}

fn cache_get_video(id: &str) -> Option<VideoRecord> {
    let mut map = VIDEO_CACHE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let now = Instant::now();
    if let Some(entry) = map.get(id) {
        if now.duration_since(entry.cached_at) <= CACHE_TTL {
            return Some(entry.record.clone());
        }
        map.remove(id);
    }
    None
}

fn cache_put_video(record: &VideoRecord) {
    let mut map = VIDEO_CACHE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let now = Instant::now();
    if map.len() >= VIDEO_CACHE_MAX {
        map.retain(|_, v| now.duration_since(v.cached_at) <= CACHE_TTL);
        if map.len() >= VIDEO_CACHE_MAX
            && let Some(oldest) = map
                .iter()
                .min_by_key(|(_, v)| v.cached_at)
                .map(|(k, _)| k.clone())
        {
            map.remove(&oldest);
        }
    }
    map.insert(
        record.id.clone(),
        CachedVideo {
            record: record.clone(),
            cached_at: now,
        },
    );
}

fn cache_get_transcript(id: &str, key: &str) -> Option<CachedTranscript> {
    let mut map = TRANSCRIPT_CACHE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let now = Instant::now();
    let k = (id.to_string(), key.to_string());
    if let Some(entry) = map.get(&k) {
        if now.duration_since(entry.cached_at) <= CACHE_TTL {
            return Some(entry.clone());
        }
        map.remove(&k);
    }
    None
}

fn cache_put_transcript(id: &str, key: &str, entry: CachedTranscript) {
    let mut map = TRANSCRIPT_CACHE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let now = Instant::now();
    if map.len() >= TRANSCRIPT_CACHE_MAX {
        map.retain(|_, v| now.duration_since(v.cached_at) <= CACHE_TTL);
        if map.len() >= TRANSCRIPT_CACHE_MAX
            && let Some(oldest) = map
                .iter()
                .min_by_key(|(_, v)| v.cached_at)
                .map(|(k, _)| k.clone())
        {
            map.remove(&oldest);
        }
    }
    map.insert((id.to_string(), key.to_string()), entry);
}

async fn post_innertube(
    http: &HttpClient,
    client_payload: Value,
    ua: &str,
    timeout: Duration,
) -> Option<Value> {
    let resp = http
        .post(INNERTUBE_PLAYER)
        .header("User-Agent", ua)
        .header("Accept", "application/json")
        .header("Content-Type", "application/json")
        .timeout(timeout)
        .json(&client_payload)
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let bytes = read_bounded(resp, MAX_PLAYER_BYTES).await.ok()?;
    serde_json::from_slice(&bytes).ok()
}

async fn read_bounded(mut resp: reqwest::Response, cap: usize) -> Result<Vec<u8>, String> {
    let mut buf = Vec::with_capacity(
        resp.content_length()
            .map(|n| (n as usize).min(cap))
            .unwrap_or(16 * 1024),
    );
    while let Some(chunk) = resp.chunk().await.map_err(|e| e.to_string())? {
        if buf.len().saturating_add(chunk.len()) > cap {
            return Err("response exceeded cap".into());
        }
        buf.extend_from_slice(&chunk);
    }
    Ok(buf)
}

/// Fast-path live YouTube video fetcher over Innertube (`IOS` + `WEB` in
/// parallel, with `ANDROID` fallback). Returns compact video cards by default
/// and fetches a single compact caption track only when `transcript_req` asks.
pub async fn fetch_live(
    url: &str,
    transcript_req: &TranscriptRequest,
    proxy: Option<&Proxy>,
    timeout: Duration,
) -> LiveOutcome {
    let t0 = Instant::now();
    let Ok(parsed) = url::Url::parse(url) else {
        return LiveOutcome::Fallback;
    };
    let Some(id) = video_id(&parsed) else {
        return LiveOutcome::Fallback;
    };
    let is_short = parsed.path().starts_with("/shorts/");
    let Ok(http) = http_client(proxy) else {
        return LiveOutcome::Fallback;
    };

    let mut video_cached = false;
    let record = if let Some(cached) = cache_get_video(&id) {
        video_cached = true;
        cached
    } else {
        let req_timeout = timeout.min(Duration::from_secs(10));
        let ios_body = json!({
            "context": {
                "client": {
                    "clientName": "IOS",
                    "clientVersion": IOS_CLIENT_VERSION,
                    "deviceMake": "Apple",
                    "deviceModel": "iPhone16,2",
                    "hl": "en",
                    "gl": "US"
                }
            },
            "videoId": id
        });
        let web_body = json!({
            "context": {
                "client": {
                    "clientName": "WEB",
                    "clientVersion": WEB_CLIENT_VERSION,
                    "hl": "en",
                    "gl": "US"
                }
            },
            "videoId": id
        });
        let (ios, web) = tokio::join!(
            post_innertube(&http, ios_body, IOS_USER_AGENT, req_timeout),
            post_innertube(&http, web_body, WEB_USER_AGENT, req_timeout),
        );

        let mut primary = ios;
        let mut client_ua = IOS_USER_AGENT;

        // If IOS didn't deliver a playable videoDetails with captions (or failed),
        // try ANDROID before giving up to HTML fallback.
        let ios_ok = primary
            .as_ref()
            .and_then(|v| v["videoDetails"]["videoId"].as_str())
            .is_some_and(|v| v == id);
        let ios_status = primary
            .as_ref()
            .and_then(|v| v["playabilityStatus"]["status"].as_str())
            .unwrap_or("");
        let ios_missing_tracks = transcript_req.is_requested()
            && primary
                .as_ref()
                .is_none_or(|v| extract_caption_tracks(v).is_empty());
        if (!ios_ok || (ios_status != "OK" && ios_status != "ERROR") || ios_missing_tracks)
            && let Some(android) = post_innertube(
                &http,
                json!({
                    "context": {
                        "client": {
                            "clientName": "ANDROID",
                            "clientVersion": ANDROID_CLIENT_VERSION,
                            "androidSdkVersion": 30,
                            "hl": "en",
                            "gl": "US"
                        }
                    },
                    "videoId": id
                }),
                ANDROID_USER_AGENT,
                req_timeout,
            )
            .await
        {
            let android_ok = android["videoDetails"]["videoId"].as_str() == Some(id.as_str());
            if android_ok || primary.is_none() {
                primary = Some(android);
                client_ua = ANDROID_USER_AGENT;
            }
        }

        // Check terminal playability failures (deleted/private/unavailable videos).
        if let Some(ref p) = primary {
            let status = p["playabilityStatus"]["status"].as_str().unwrap_or("");
            let reason = p["playabilityStatus"]["reason"]
                .as_str()
                .unwrap_or("This video is unavailable")
                .trim()
                .to_string();
            let has_title = p["videoDetails"]["title"]
                .as_str()
                .is_some_and(|t| !t.trim().is_empty());
            if status == "ERROR" && !has_title {
                return LiveOutcome::Unavailable {
                    status: 404,
                    reason,
                    auth_required: false,
                };
            }
            if status == "LOGIN_REQUIRED" && !has_title {
                let low = reason.to_ascii_lowercase();
                if low.contains("private") || low.contains("members-only") {
                    return LiveOutcome::Unavailable {
                        status: 403,
                        reason,
                        auth_required: true,
                    };
                }
                return LiveOutcome::Fallback;
            }
        }

        let Some(rec) = build_record(&id, primary.as_ref(), web.as_ref(), client_ua) else {
            return LiveOutcome::Fallback;
        };
        cache_put_video(&rec);
        rec
    };

    let available_tracks = compact_track_codes(&record.tracks);

    // Fetch transcript ONLY when explicitly requested by the model/caller.
    let (transcript_section, transcript_meta, transcript_cached) = if transcript_req.is_requested()
    {
        fetch_transcript_section(&http, &record, transcript_req, &available_tracks, timeout).await
    } else {
        (None, None, false)
    };

    let md = render_video_markdown(
        &record,
        is_short,
        &available_tracks,
        transcript_section.as_deref(),
        transcript_req.is_requested(),
    );
    let lang = crate::extract::language::detect_from_text(&md);
    let total = md.len();
    let blocks = 2
        + usize::from(!record.chapters.is_empty())
        + usize::from(!record.description.is_empty())
        + usize::from(transcript_section.is_some());

    let extracted = Extracted {
        page: None,
        tokens_est: md.len() / 4,
        markdown: md,
        title: Some(record.title.clone()),
        byline: record.author.clone(),
        published: record.published.clone(),
        site: Some("YouTube".to_string()),
        total_chars: total,
        next_offset: None,
        blocks_total: blocks,
        blocks_shown: blocks,
        focus_omitted: false,
        thin: false,
        content_kind: ContentKind::Article,
        lang: lang.code,
        lang_source: None,
        script: Some(lang.script),
        quality: 0.95,
        pdf_pages: None,
        images: Vec::new(),
        fingerprint: None,
        via: Some("adapter:youtube"),
        partial: None,
    };

    LiveOutcome::Video(Box::new(LiveVideo {
        extracted,
        transcript_meta,
        available_tracks,
        cached: video_cached && (!transcript_req.is_requested() || transcript_cached),
        elapsed_ms: t0.elapsed().as_millis(),
    }))
}

async fn fetch_transcript_section(
    http: &HttpClient,
    record: &VideoRecord,
    req: &TranscriptRequest,
    available_tracks: &[String],
    timeout: Duration,
) -> (Option<String>, Option<Value>, bool) {
    if record.tracks.is_empty() {
        return (
            None,
            Some(json!({
                "status": "unavailable",
                "reason": "this video has no captions or transcript tracks",
                "tracks_available": [],
            })),
            false,
        );
    }

    let Some((track, translate_to)) = select_track(&record.tracks, req) else {
        return (
            None,
            Some(json!({
                "status": "unavailable",
                "reason": "no matching transcript track available",
                "tracks_available": available_tracks,
            })),
            false,
        );
    };

    let out_lang = translate_to.as_deref().unwrap_or(track.lang.as_str());
    let cache_key = format!(
        "{}:{}:{}",
        out_lang.to_ascii_lowercase(),
        track.auto_generated,
        translate_to.is_some()
    );
    if let Some(hit) = cache_get_transcript(&record.id, &cache_key) {
        return (
            Some(hit.markdown),
            Some(json!({
                "status": "ok",
                "lang": hit.lang,
                "auto_generated": hit.auto_generated,
                "translated": hit.translated,
                "cues": hit.cues,
                "cached": true,
                "tracks_available": available_tracks,
            })),
            true,
        );
    }

    let req_timeout = timeout.min(Duration::from_secs(12));
    let target_url = caption_url(&track.base_url, translate_to.as_deref(), true);
    let raw = match fetch_caption_bytes(http, &target_url, record.client_ua, req_timeout).await {
        Ok(bytes) => Ok(bytes),
        Err(_) if translate_to.is_none() => {
            // One retry against the track's verbatim baseUrl if normalizing to srv1 failed.
            fetch_caption_bytes(http, &track.base_url, record.client_ua, req_timeout).await
        }
        Err(err) => Err(err),
    };

    let bytes = match raw {
        Ok(b) => b,
        Err(reason) => {
            return (
                None,
                Some(json!({
                    "status": "error",
                    "lang": out_lang,
                    "reason": reason,
                    "tracks_available": available_tracks,
                })),
                false,
            );
        }
    };

    let body = String::from_utf8_lossy(&bytes);
    let cues = parse_captions(&body);
    if cues.is_empty() {
        return (
            None,
            Some(json!({
                "status": "unavailable",
                "lang": out_lang,
                "reason": "caption track contained no spoken text",
                "tracks_available": available_tracks,
            })),
            false,
        );
    }

    let cue_count = cues.len();
    let formatted = format_transcript(
        &cues,
        &record.chapters,
        out_lang,
        &track.name,
        track.auto_generated,
        translate_to.is_some(),
    );

    cache_put_transcript(
        &record.id,
        &cache_key,
        CachedTranscript {
            markdown: formatted.clone(),
            lang: out_lang.to_string(),
            auto_generated: track.auto_generated,
            translated: translate_to.is_some(),
            cues: cue_count,
            cached_at: Instant::now(),
        },
    );

    (
        Some(formatted),
        Some(json!({
            "status": "ok",
            "lang": out_lang,
            "auto_generated": track.auto_generated,
            "translated": translate_to.is_some(),
            "cues": cue_count,
            "tracks_available": available_tracks,
        })),
        false,
    )
}

async fn fetch_caption_bytes(
    http: &HttpClient,
    url: &str,
    ua: &str,
    timeout: Duration,
) -> Result<Vec<u8>, String> {
    let resp = http
        .get(url)
        .header("User-Agent", ua)
        .timeout(timeout)
        .send()
        .await
        .map_err(|e| format!("caption fetch failed: {}", e.without_url()))?;
    if !resp.status().is_success() {
        return Err(format!(
            "caption endpoint returned HTTP {}",
            resp.status().as_u16()
        ));
    }
    read_bounded(resp, MAX_CAPTION_BYTES).await
}

fn build_record(
    id: &str,
    primary: Option<&Value>,
    web: Option<&Value>,
    client_ua: &'static str,
) -> Option<VideoRecord> {
    let p_details = primary.map(|v| &v["videoDetails"]);
    let w_details = web.map(|v| &v["videoDetails"]);
    let details = match (p_details, w_details) {
        (Some(d), _) if d["videoId"].as_str() == Some(id) => d,
        (_, Some(d)) if d["videoId"].as_str() == Some(id) => d,
        _ => return None,
    };
    let title = details["title"].as_str()?.trim();
    if title.is_empty() {
        return None;
    }

    let w_micro = web.map(|v| &v["microformat"]["playerMicroformatRenderer"]);
    let p_micro = primary.map(|v| &v["microformat"]["playerMicroformatRenderer"]);
    let micro_str = |key: &str| -> Option<String> {
        w_micro
            .and_then(|m| m[key].as_str())
            .or_else(|| p_micro.and_then(|m| m[key].as_str()))
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };

    let author = details["author"]
        .as_str()
        .map(str::trim)
        .filter(|a| !a.is_empty())
        .map(str::to_string)
        .or_else(|| micro_str("ownerChannelName"));

    let published = micro_str("publishDate")
        .or_else(|| micro_str("uploadDate"))
        .map(|d| d.get(..10).unwrap_or(&d).to_string());

    let is_live = w_micro
        .and_then(|m| m["liveBroadcastDetails"]["isLiveNow"].as_bool())
        .or_else(|| p_micro.and_then(|m| m["liveBroadcastDetails"]["isLiveNow"].as_bool()))
        .or_else(|| details["isLive"].as_bool())
        .unwrap_or(false);

    let length_seconds = details["lengthSeconds"]
        .as_str()
        .and_then(|s| s.parse::<u64>().ok())
        .filter(|s| *s > 0);

    let views = details["viewCount"]
        .as_str()
        .and_then(|v| v.parse::<u64>().ok())
        .or_else(|| {
            w_details
                .and_then(|d| d["viewCount"].as_str())
                .and_then(|v| v.parse::<u64>().ok())
        });

    let likes = micro_str("likeCount").and_then(|v| v.parse::<u64>().ok());
    let category = micro_str("category");
    let is_unlisted = w_micro
        .and_then(|m| m["isUnlisted"].as_bool())
        .or_else(|| p_micro.and_then(|m| m["isUnlisted"].as_bool()))
        .unwrap_or(false);

    let description = details["shortDescription"]
        .as_str()
        .or_else(|| w_details.and_then(|d| d["shortDescription"].as_str()))
        .unwrap_or("")
        .trim()
        .to_string();

    let chapters = parse_description_chapters(&description, length_seconds);

    let mut tracks = primary.map(extract_caption_tracks).unwrap_or_default();
    if tracks.is_empty()
        && let Some(w) = web
    {
        tracks = extract_caption_tracks(w);
    }

    Some(VideoRecord {
        id: id.to_string(),
        title: title.to_string(),
        author,
        published,
        length_seconds,
        is_live,
        views,
        likes,
        category,
        is_unlisted,
        description,
        chapters,
        tracks,
        client_ua,
    })
}

fn is_youtube_caption_url(raw: &str) -> bool {
    let Ok(parsed) = url::Url::parse(raw) else {
        return false;
    };
    if parsed.scheme() != "https" {
        return false;
    }
    let Some(host) = parsed.host_str().map(str::to_ascii_lowercase) else {
        return false;
    };
    matches!(
        host.as_str(),
        "youtube.com" | "www.youtube.com" | "m.youtube.com" | "music.youtube.com"
    ) || host.ends_with(".youtube.com")
        || host.ends_with(".googlevideo.com")
        || host.ends_with(".google.com")
}

fn extract_caption_tracks(player: &Value) -> Vec<CaptionTrack> {
    let Some(list) =
        player["captions"]["playerCaptionsTracklistRenderer"]["captionTracks"].as_array()
    else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for item in list {
        let (Some(lang), Some(base_url)) = (
            item["languageCode"].as_str().map(str::trim),
            item["baseUrl"].as_str().map(str::trim),
        ) else {
            continue;
        };
        if lang.is_empty() || !is_youtube_caption_url(base_url) {
            continue;
        }
        let name = text(&item["name"]).unwrap_or_else(|| lang.to_string());
        let auto_generated = item["kind"].as_str() == Some("asr");
        let translatable = item["isTranslatable"].as_bool().unwrap_or(false);
        out.push(CaptionTrack {
            lang: lang.to_string(),
            name,
            base_url: base_url.to_string(),
            auto_generated,
            translatable,
        });
    }
    out
}

fn compact_track_codes(tracks: &[CaptionTrack]) -> Vec<String> {
    let mut out = Vec::new();
    // Put manual English first if present, then manual tracks, then auto tracks.
    let mut ordered: Vec<&CaptionTrack> = tracks.iter().collect();
    ordered.sort_by_key(|t| {
        let is_en =
            t.lang.eq_ignore_ascii_case("en") || t.lang.to_ascii_lowercase().starts_with("en-");
        (t.auto_generated, !is_en)
    });
    for t in ordered {
        let label = if t.auto_generated {
            format!("{} (auto)", t.lang)
        } else {
            t.lang.clone()
        };
        if !out.contains(&label) {
            out.push(label);
            if out.len() >= MAX_META_TRACKS {
                break;
            }
        }
    }
    out
}

/// Choose the best caption track for the caller's request.
/// Returns `(track, optional_tlang_target)` when auto-translation is needed.
fn select_track<'a>(
    tracks: &'a [CaptionTrack],
    req: &TranscriptRequest,
) -> Option<(&'a CaptionTrack, Option<String>)> {
    if tracks.is_empty() {
        return None;
    }
    let is_lang_prefix = |track_lang: &str, wanted: &str| {
        let tl = track_lang.to_ascii_lowercase();
        let wl = wanted.to_ascii_lowercase();
        tl == wl
            || tl
                .strip_prefix(&wl)
                .is_some_and(|rest| rest.starts_with('-'))
            || wl
                .strip_prefix(&tl)
                .is_some_and(|rest| rest.starts_with('-'))
    };

    match req {
        TranscriptRequest::Off => None,
        TranscriptRequest::Lang(wanted) => {
            // 1. Exact manual match, then exact auto-generated match.
            if let Some(t) = tracks
                .iter()
                .find(|t| !t.auto_generated && t.lang.eq_ignore_ascii_case(wanted))
            {
                return Some((t, None));
            }
            if let Some(t) = tracks
                .iter()
                .find(|t| t.auto_generated && t.lang.eq_ignore_ascii_case(wanted))
            {
                return Some((t, None));
            }
            // 2. Language prefix match (e.g. "en" -> "en-US", "pt" -> "pt-BR").
            if let Some(t) = tracks
                .iter()
                .find(|t| !t.auto_generated && is_lang_prefix(&t.lang, wanted))
            {
                return Some((t, None));
            }
            if let Some(t) = tracks
                .iter()
                .find(|t| t.auto_generated && is_lang_prefix(&t.lang, wanted))
            {
                return Some((t, None));
            }
            // 3. Auto-translate from the best translatable track (prefer English manual -> any manual -> English auto -> any).
            let source = tracks
                .iter()
                .find(|t| t.translatable && !t.auto_generated && is_lang_prefix(&t.lang, "en"))
                .or_else(|| tracks.iter().find(|t| t.translatable && !t.auto_generated))
                .or_else(|| {
                    tracks
                        .iter()
                        .find(|t| t.translatable && is_lang_prefix(&t.lang, "en"))
                })
                .or_else(|| tracks.iter().find(|t| t.translatable))?;
            Some((source, Some(wanted.clone())))
        }
        TranscriptRequest::Default => {
            // Prefer manual English -> any manual track -> auto English -> first track.
            let best = tracks
                .iter()
                .find(|t| !t.auto_generated && is_lang_prefix(&t.lang, "en"))
                .or_else(|| tracks.iter().find(|t| !t.auto_generated))
                .or_else(|| tracks.iter().find(|t| is_lang_prefix(&t.lang, "en")))
                .or_else(|| tracks.first())?;
            Some((best, None))
        }
    }
}

/// Normalize a timedtext `baseUrl` to compact `fmt=srv1` (and optional `tlang`).
fn caption_url(base_url: &str, tlang: Option<&str>, prefer_srv1: bool) -> String {
    let Ok(mut parsed) = url::Url::parse(base_url) else {
        return base_url.to_string();
    };
    let pairs: Vec<(String, String)> = parsed
        .query_pairs()
        .filter(|(k, _)| {
            let keep_fmt = !prefer_srv1 || k != "fmt";
            let keep_tlang = tlang.is_none() || k != "tlang";
            keep_fmt && keep_tlang
        })
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    {
        let mut q = parsed.query_pairs_mut();
        q.clear();
        for (k, v) in pairs {
            q.append_pair(&k, &v);
        }
        if prefer_srv1 {
            q.append_pair("fmt", "srv1");
        }
        if let Some(target) = tlang {
            q.append_pair("tlang", target);
        }
    }
    parsed.to_string()
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Cue {
    start_secs: u64,
    text: String,
}

/// Parse YouTube timedtext captions in `srv1` XML (`<text start="...">`),
/// `srv3` XML (`<p t="...">`), or `json3` (`{"events":[...]}`).
fn parse_captions(raw: &str) -> Vec<Cue> {
    let trimmed = raw.trim_start();
    if trimmed.starts_with('{') {
        return parse_json3_captions(trimmed);
    }
    if trimmed.contains("<text ") || trimmed.contains("<text>") {
        let cues = parse_srv1_captions(trimmed);
        if !cues.is_empty() {
            return cues;
        }
    }
    parse_srv3_captions(trimmed)
}

fn parse_srv1_captions(xml: &str) -> Vec<Cue> {
    let mut cues = Vec::new();
    let mut rest = xml;
    while let Some(open) = rest.find("<text") {
        rest = &rest[open + 5..];
        let Some(tag_end) = rest.find('>') else {
            break;
        };
        let attrs = &rest[..tag_end];
        rest = &rest[tag_end + 1..];
        if attrs.ends_with('/') {
            continue;
        }
        let Some(close) = rest.find("</text>") else {
            break;
        };
        let inner = &rest[..close];
        rest = &rest[close + 7..];
        let start_secs = xml_attr(attrs, "start")
            .and_then(|s| s.parse::<f64>().ok())
            .map(|s| s.max(0.0) as u64)
            .unwrap_or(0);
        let text = clean_cue_text(inner);
        if !text.is_empty() {
            cues.push(Cue { start_secs, text });
        }
    }
    dedup_rolling_cues(cues)
}

fn parse_srv3_captions(xml: &str) -> Vec<Cue> {
    let mut cues = Vec::new();
    let mut rest = xml;
    while let Some(open) = rest.find("<p ") {
        rest = &rest[open + 3..];
        let Some(tag_end) = rest.find('>') else {
            break;
        };
        let attrs = &rest[..tag_end];
        rest = &rest[tag_end + 1..];
        if attrs.ends_with('/') {
            continue;
        }
        let Some(close) = rest.find("</p>") else {
            break;
        };
        let inner = &rest[..close];
        rest = &rest[close + 4..];
        let start_secs = xml_attr(attrs, "t")
            .and_then(|s| s.parse::<u64>().ok())
            .map(|ms| ms / 1000)
            .unwrap_or(0);
        let text = clean_cue_text(inner);
        if !text.is_empty() {
            cues.push(Cue { start_secs, text });
        }
    }
    dedup_rolling_cues(cues)
}

fn parse_json3_captions(raw: &str) -> Vec<Cue> {
    let Ok(val) = serde_json::from_str::<Value>(raw) else {
        return Vec::new();
    };
    let Some(events) = val["events"].as_array() else {
        return Vec::new();
    };
    let mut cues = Vec::new();
    for ev in events {
        let Some(segs) = ev["segs"].as_array() else {
            continue;
        };
        let start_secs = ev["tStartMs"].as_u64().unwrap_or(0) / 1000;
        let joined: String = segs.iter().filter_map(|s| s["utf8"].as_str()).collect();
        let text = clean_cue_text(&joined);
        if !text.is_empty() {
            cues.push(Cue { start_secs, text });
        }
    }
    dedup_rolling_cues(cues)
}

fn xml_attr<'a>(attrs: &'a str, name: &str) -> Option<&'a str> {
    let pat_dq = format!("{name}=\"");
    if let Some(i) = attrs.find(&pat_dq) {
        let rest = &attrs[i + pat_dq.len()..];
        if let Some(end) = rest.find('"') {
            return Some(&rest[..end]);
        }
    }
    let pat_sq = format!("{name}='");
    if let Some(i) = attrs.find(&pat_sq) {
        let rest = &attrs[i + pat_sq.len()..];
        if let Some(end) = rest.find('\'') {
            return Some(&rest[..end]);
        }
    }
    None
}

fn clean_cue_text(raw: &str) -> String {
    // Strip inline XML tags like `<s t="120">` or `<font ...>` from srv3/srv1.
    let mut no_tags = String::with_capacity(raw.len());
    let mut in_tag = false;
    for ch in raw.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => no_tags.push(ch),
            _ => {}
        }
    }
    // Decode HTML entities twice because YouTube frequently double-encodes
    // (`&amp;#39;` -> `&#39;` -> `'`).
    let decoded = decode_entities(&decode_entities(&no_tags));
    // Collapse whitespace/newlines into single spaces.
    decoded.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn decode_entities(input: &str) -> String {
    if !input.contains('&') {
        return input.to_string();
    }
    let mut out = String::with_capacity(input.len());
    let bytes = input.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'&'
            && let Some(semi_rel) = input[i..].find(';')
            && semi_rel <= 12
        {
            let entity = &input[i + 1..i + semi_rel];
            let replacement: Option<char> = match entity {
                "amp" => Some('&'),
                "lt" => Some('<'),
                "gt" => Some('>'),
                "quot" => Some('"'),
                "apos" => Some('\''),
                "nbsp" => Some(' '),
                s if s.starts_with("#x") || s.starts_with("#X") => u32::from_str_radix(&s[2..], 16)
                    .ok()
                    .and_then(char::from_u32),
                s if s.starts_with('#') => s[1..].parse::<u32>().ok().and_then(char::from_u32),
                _ => None,
            };
            if let Some(ch) = replacement {
                out.push(ch);
                i += semi_rel + 1;
                continue;
            }
        }
        let ch = input[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

fn dedup_rolling_cues(cues: Vec<Cue>) -> Vec<Cue> {
    let mut out: Vec<Cue> = Vec::with_capacity(cues.len());
    for cue in cues {
        if let Some(prev) = out.last()
            && prev.text.eq_ignore_ascii_case(&cue.text)
        {
            continue;
        }
        out.push(cue);
    }
    out
}

/// Group short timed captions into compact, readable timestamped paragraphs
/// (aligned with video chapters when present).
fn format_transcript(
    cues: &[Cue],
    chapters: &[(String, String)],
    lang: &str,
    track_name: &str,
    auto_generated: bool,
    translated: bool,
) -> String {
    let label = if translated {
        format!("{lang}, translated")
    } else if auto_generated {
        format!("{lang}, auto-generated")
    } else if !track_name.is_empty() && !track_name.eq_ignore_ascii_case(lang) {
        format!("{lang} — {track_name}")
    } else {
        lang.to_string()
    };

    let chapter_marks: Vec<(u64, &str)> = chapters
        .iter()
        .filter_map(|(at, name)| parse_timestamp_secs(at).map(|s| (s, name.as_str())))
        .collect();

    let mut out = format!("## Transcript ({label})\n\n");
    let mut next_chapter = 0usize;
    let mut para_start: Option<u64> = None;
    let mut para = String::new();

    let flush = |out: &mut String, start: &mut Option<u64>, para: &mut String| {
        if let Some(s) = start.take() {
            let trimmed = para.trim();
            if !trimmed.is_empty() {
                out.push_str(&format!("[{}] {}\n\n", duration(s), trimmed));
            }
            para.clear();
        }
    };

    for cue in cues {
        // Emit chapter boundary heading when the transcript crosses into a new chapter.
        while next_chapter < chapter_marks.len() && cue.start_secs >= chapter_marks[next_chapter].0
        {
            flush(&mut out, &mut para_start, &mut para);
            let (ch_secs, ch_title) = chapter_marks[next_chapter];
            out.push_str(&format!("### {} {}\n\n", duration(ch_secs), ch_title));
            next_chapter += 1;
        }

        if para_start.is_none() {
            para_start = Some(cue.start_secs);
        }
        if !para.is_empty() {
            para.push(' ');
        }
        para.push_str(&cue.text);

        let elapsed = cue.start_secs.saturating_sub(para_start.unwrap_or(0));
        let ends_sentence = para.ends_with('.') || para.ends_with('?') || para.ends_with('!');
        if para.len() >= 480 || (para.len() >= 240 && ends_sentence) || elapsed >= 45 {
            flush(&mut out, &mut para_start, &mut para);
        }
    }
    flush(&mut out, &mut para_start, &mut para);
    out.trim_end().to_string()
}

fn render_video_markdown(
    record: &VideoRecord,
    is_short: bool,
    available_tracks: &[String],
    transcript_section: Option<&str>,
    transcript_requested: bool,
) -> String {
    let mut facts: Vec<String> = Vec::new();
    facts.extend(record.author.clone());
    facts.extend(record.published.clone());
    if record.is_live {
        facts.push("live now".to_string());
    } else if let Some(seconds) = record.length_seconds {
        facts.push(duration(seconds));
    }
    if is_short {
        facts.push("Shorts".to_string());
    }
    if let Some(views) = record.views {
        facts.push(format!("{} views", thousands(views)));
    }
    if let Some(likes) = record.likes.filter(|l| *l > 0) {
        facts.push(format!("{} likes", thousands(likes)));
    }
    facts.extend(record.category.clone());
    if record.is_unlisted {
        facts.push("Unlisted".to_string());
    }

    let mut md = format!("# {}\n\n", record.title);
    if !facts.is_empty() {
        md.push_str(&facts.join(" · "));
        md.push_str("\n\n");
    }
    if !record.chapters.is_empty() {
        md.push_str("## Chapters\n\n");
        for (at, name) in &record.chapters {
            md.push_str(&format!("- {at} {name}\n"));
        }
        md.push('\n');
    }
    if !record.description.is_empty() {
        md.push_str("## Description\n\n");
        for line in record.description.lines() {
            let trimmed_start = line.trim_start();
            if trimmed_start.starts_with("# ")
                || trimmed_start.starts_with("## ")
                || trimmed_start.starts_with("### ")
            {
                md.push('\\');
            }
            md.push_str(line.trim_end());
            md.push('\n');
        }
        md.push('\n');
    }
    if let Some(transcript) = transcript_section {
        md.push_str(transcript);
        md.push('\n');
    } else if !transcript_requested && !available_tracks.is_empty() {
        let shown: Vec<&str> = available_tracks
            .iter()
            .take(6)
            .map(String::as_str)
            .collect();
        let extra = available_tracks.len().saturating_sub(shown.len());
        let suffix = if extra > 0 {
            format!(" (+{extra} more)")
        } else {
            String::new()
        };
        md.push_str(&format!(
            "> Transcript available: {}{suffix} — re-fetch with `transcript: true` (or a language code) to read it.\n",
            shown.join(", ")
        ));
    }
    md.trim_end().to_string()
}

/// Parse chapter timestamps from a video description (e.g. `0:00 Intro`,
/// `[04:12] Deep dive`, `1:02:05 - Conclusion`). Requires at least 2 ascending
/// timestamps inside the video's length so stray time mentions aren't mistaken
/// for chapters.
pub(crate) fn parse_description_chapters(
    description: &str,
    length_seconds: Option<u64>,
) -> Vec<(String, String)> {
    let mut found: Vec<(u64, String, String)> = Vec::new();
    for raw_line in description.lines() {
        let line = raw_line.trim();
        if line.is_empty() {
            continue;
        }
        let cleaned = line
            .trim_start_matches(['-', '*', '•', '(', '['])
            .trim_start();
        let Some((ts_raw, after)) = split_leading_timestamp(cleaned) else {
            continue;
        };
        let Some(secs) = parse_timestamp_secs(ts_raw) else {
            continue;
        };
        if let Some(len) = length_seconds
            && secs > len
        {
            continue;
        }
        let title = after
            .trim_start_matches([')', ']', '-', '–', '—', ':', '|', ' '])
            .trim();
        if title.is_empty() || title.len() > 160 {
            continue;
        }
        if let Some((last_secs, _, _)) = found.last()
            && secs < *last_secs
        {
            continue;
        }
        let formatted_ts = duration(secs);
        if !found
            .iter()
            .any(|(_, a, n)| *a == formatted_ts && *n == title)
        {
            found.push((secs, formatted_ts, title.to_string()));
        }
    }
    if found.len() < 2 {
        return Vec::new();
    }
    found.into_iter().map(|(_, at, name)| (at, name)).collect()
}

fn split_leading_timestamp(s: &str) -> Option<(&str, &str)> {
    let end = s
        .find(|c: char| !(c.is_ascii_digit() || c == ':'))
        .unwrap_or(s.len());
    let ts = &s[..end];
    if !ts.contains(':') {
        return None;
    }
    let rest = &s[end..];
    // Require a delimiter after the timestamp (whitespace, bracket, dash, colon).
    if !rest.is_empty() && !rest.starts_with([' ', '\t', ')', ']', '-', '–', '—', ':', '|']) {
        return None;
    }
    Some((ts, rest))
}

fn parse_timestamp_secs(ts: &str) -> Option<u64> {
    let parts: Vec<&str> = ts.split(':').collect();
    match parts.as_slice() {
        [m, s] if (1..=2).contains(&m.len()) && s.len() == 2 => {
            let mm: u64 = m.parse().ok()?;
            let ss: u64 = s.parse().ok()?;
            (ss < 60).then_some(mm * 60 + ss)
        }
        [h, m, s] if (1..=2).contains(&h.len()) && m.len() == 2 && s.len() == 2 => {
            let hh: u64 = h.parse().ok()?;
            let mm: u64 = m.parse().ok()?;
            let ss: u64 = s.parse().ok()?;
            (mm < 60 && ss < 60).then_some(hh * 3600 + mm * 60 + ss)
        }
        _ => None,
    }
}

/// HTML watch-page fallback extractor: reads `ytInitialPlayerResponse` and
/// `ytInitialData` when a watch page was fetched directly (e.g. crawl or
/// Innertube fallback).
pub fn extract(html: &str, url: &str, opts: &ExtractOptions) -> Option<Extracted> {
    let parsed = url::Url::parse(url).ok()?;
    let id = video_id(&parsed)?;
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
    let is_live = micro["liveBroadcastDetails"]["isLiveNow"].as_bool() == Some(true);
    let length_seconds = details["lengthSeconds"]
        .as_str()
        .and_then(|s| s.parse::<u64>().ok())
        .filter(|s| *s > 0);
    let views = details["viewCount"]
        .as_str()
        .and_then(|v| v.parse::<u64>().ok());
    let likes = micro["likeCount"]
        .as_str()
        .and_then(|v| v.parse::<u64>().ok());
    let category = micro["category"].as_str().map(str::to_string);
    let is_unlisted = micro["isUnlisted"].as_bool().unwrap_or(false);
    let description = details["shortDescription"]
        .as_str()
        .unwrap_or("")
        .trim()
        .to_string();

    let mut chaps = embedded(html, "ytInitialData = ")
        .map(|data| chapters(&data))
        .unwrap_or_default();
    if chaps.is_empty() {
        chaps = parse_description_chapters(&description, length_seconds);
    }

    let tracks = extract_caption_tracks(&player);
    let available = compact_track_codes(&tracks);

    let record = VideoRecord {
        id,
        title: title.to_string(),
        author: author.clone(),
        published: published.clone(),
        length_seconds,
        is_live,
        views,
        likes,
        category,
        is_unlisted,
        description,
        chapters: chaps,
        tracks,
        client_ua: WEB_USER_AGENT,
    };
    let is_short = parsed.path().starts_with("/shorts/");
    let md = render_video_markdown(&record, is_short, &available, None, false);

    let blocks =
        2 + usize::from(!record.chapters.is_empty()) + usize::from(!record.description.is_empty());
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

/// The video a YouTube URL names: `/watch?v=`, `/shorts/`, `/live/`, `/embed/`,
/// `/v/`, `youtu.be/<id>`, or `youtube-nocookie.com/embed/<id>`, as an
/// 11-character video id.
pub fn video_id(u: &url::Url) -> Option<String> {
    let host = u.host_str()?.to_ascii_lowercase();
    let host = host
        .strip_prefix("www.")
        .or_else(|| host.strip_prefix("m."))
        .unwrap_or(&host);
    let segments: Vec<&str> = u.path_segments()?.filter(|s| !s.is_empty()).collect();
    let id = match (host, segments.as_slice()) {
        ("youtube.com" | "music.youtube.com", ["watch"]) => {
            u.query_pairs().find(|(k, _)| k == "v")?.1.into_owned()
        }
        ("youtube.com", ["shorts" | "live" | "embed" | "v", id])
        | ("youtube-nocookie.com", ["embed", id])
        | ("youtu.be", [id]) => (*id).to_string(),
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
            "https://www.youtube.com/embed/dQw4w9WgXcQ",
            "https://www.youtube-nocookie.com/embed/dQw4w9WgXcQ",
            "https://www.youtube.com/v/dQw4w9WgXcQ",
            "https://www.youtube.com/live/dQw4w9WgXcQ",
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

    #[test]
    fn description_chapters_parse_cleanly() {
        let desc = "Check out the code!\n\n0:00 Intro & setup\n04:12 - Parsing tokens\n[1:02:05] Final benchmark\n";
        let ch = parse_description_chapters(desc, Some(4000));
        assert_eq!(
            ch,
            vec![
                ("0:00".into(), "Intro & setup".into()),
                ("4:12".into(), "Parsing tokens".into()),
                ("1:02:05".into(), "Final benchmark".into()),
            ]
        );
        // Single timestamp is not a chapter list.
        assert!(parse_description_chapters("At 1:23 I sneeze.", Some(400)).is_empty());
    }

    #[test]
    fn captions_parse_all_three_youtube_formats_and_decode_entities() {
        let srv1 = r#"<?xml version="1.0" encoding="utf-8" ?><transcript><text start="0.2" dur="3.1">Hello &amp;#39;world&amp;#39; &amp; friends</text><text start="3.5" dur="2.0">Hello &#39;world&#39; &amp; friends</text><text start="6.0" dur="2.5">Next line.</text></transcript>"#;
        let cues = parse_captions(srv1);
        assert_eq!(cues.len(), 2);
        assert_eq!(cues[0].text, "Hello 'world' & friends");
        assert_eq!(cues[1].start_secs, 6);
        assert_eq!(cues[1].text, "Next line.");

        let srv3 = r#"<timedtext format="3"><body><p t="18640" d="3240"><s>We&#39;re</s> <s>no strangers</s></p></body></timedtext>"#;
        let cues3 = parse_captions(srv3);
        assert_eq!(cues3.len(), 1);
        assert_eq!(cues3[0].start_secs, 18);
        assert_eq!(cues3[0].text, "We're no strangers");

        let json3 = r#"{"events":[{"tStartMs":12500,"segs":[{"utf8":"From "},{"utf8":"JSON3"}]}]}"#;
        let cuesj = parse_captions(json3);
        assert_eq!(cuesj.len(), 1);
        assert_eq!(cuesj[0].start_secs, 12);
        assert_eq!(cuesj[0].text, "From JSON3");
    }

    #[test]
    fn track_selection_prefers_manual_english_and_honors_explicit_language() {
        let mk = |lang: &str, auto: bool, tr: bool| CaptionTrack {
            lang: lang.into(),
            name: lang.into(),
            base_url: format!("https://www.youtube.com/api/timedtext?lang={lang}"),
            auto_generated: auto,
            translatable: tr,
        };
        let tracks = vec![
            mk("ar", false, true),
            mk("en", true, true),
            mk("en", false, true),
            mk("es-419", false, true),
        ];
        // Default picks manual English over alphabetical Arabic and auto English.
        let (t, tr) = select_track(&tracks, &TranscriptRequest::Default).unwrap();
        assert_eq!((t.lang.as_str(), t.auto_generated, tr), ("en", false, None));
        // Prefix match for "es" picks "es-419".
        let (t, tr) = select_track(&tracks, &TranscriptRequest::Lang("es".into())).unwrap();
        assert_eq!((t.lang.as_str(), tr), ("es-419", None));
        // Unlisted language falls back to auto-translation from English manual track.
        let (t, tr) = select_track(&tracks, &TranscriptRequest::Lang("ja".into())).unwrap();
        assert_eq!((t.lang.as_str(), tr.as_deref()), ("en", Some("ja")));
    }

    #[test]
    fn transcript_formatting_groups_paragraphs_and_aligns_chapters() {
        let cues = vec![
            Cue {
                start_secs: 0,
                text: "Welcome to the talk.".into(),
            },
            Cue {
                start_secs: 5,
                text: "Today we build a tokenizer.".into(),
            },
            Cue {
                start_secs: 120,
                text: "Now let's write self-attention.".into(),
            },
        ];
        let chapters = vec![
            ("0:00".into(), "Intro".into()),
            ("2:00".into(), "Attention".into()),
        ];
        let md = format_transcript(&cues, &chapters, "en", "English", false, false);
        assert!(md.starts_with("## Transcript (en — English)\n\n"), "{md}");
        assert!(
            md.contains(
                "### 0:00 Intro\n\n[0:00] Welcome to the talk. Today we build a tokenizer."
            ),
            "{md}"
        );
        assert!(
            md.contains("### 2:00 Attention\n\n[2:00] Now let's write self-attention."),
            "{md}"
        );
    }

    #[test]
    fn parse_transcript_arg_accepts_bool_and_lang_codes_only() {
        assert_eq!(parse_transcript_arg(None).unwrap(), TranscriptRequest::Off);
        assert_eq!(
            parse_transcript_arg(Some(&json!(false))).unwrap(),
            TranscriptRequest::Off
        );
        assert_eq!(
            parse_transcript_arg(Some(&json!(true))).unwrap(),
            TranscriptRequest::Default
        );
        assert_eq!(
            parse_transcript_arg(Some(&json!("pt_BR"))).unwrap(),
            TranscriptRequest::Lang("pt-BR".into())
        );
        assert!(parse_transcript_arg(Some(&json!(""))).is_err());
        assert!(parse_transcript_arg(Some(&json!("en; rm -rf"))).is_err());
    }
}
