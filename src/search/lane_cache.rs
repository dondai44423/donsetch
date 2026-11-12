//! Per-source answer cache. Only judged, nonempty answers may be replayed.

use super::{EngineResult, TaskFut, engines::AnswerClass, tasks::LaneAnswer};
use std::collections::HashMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const CAP: usize = 256;
const MAX_FILE: u64 = 4 << 20;

#[derive(serde::Serialize, serde::Deserialize)]
struct Entry {
    written: u64,
    ttl: u64,
    quality: AnswerClass,
    hits: Vec<super::engines::Hit>,
}

#[derive(Default, serde::Serialize, serde::Deserialize)]
pub(super) struct LaneCache {
    entries: HashMap<String, Entry>,
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn path() -> std::path::PathBuf {
    crate::paths::cache_dir().join("search-lanes.json")
}

impl LaneCache {
    pub fn load() -> Self {
        if crate::config::cfg().state.no_disk_state {
            return Self::default();
        }
        let path = path();
        if !std::fs::metadata(&path).is_ok_and(|m| m.len() <= MAX_FILE) {
            return Self::default();
        }
        std::fs::read(path)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }

    pub fn save(&mut self) {
        let time = now();
        self.entries.retain(|_, e| valid(e, time));
        while self.entries.len() > CAP {
            let Some(key) = self
                .entries
                .iter()
                .min_by_key(|(_, e)| e.written)
                .map(|(k, _)| k.clone())
            else {
                break;
            };
            self.entries.remove(&key);
        }
        if crate::config::cfg().state.no_disk_state {
            return;
        }
        let path = path();
        let tmp = path.with_extension(format!("{}.tmp", std::process::id()));
        if let Ok(bytes) = serde_json::to_vec(self)
            && bytes.len() as u64 <= MAX_FILE
            && crate::config::write_private(&tmp, &bytes).is_ok()
        {
            let _ = std::fs::rename(&tmp, path);
        }
    }

    pub fn clear(&mut self) {
        self.entries.clear();
    }

    pub fn contains(&self, source: &str, query: &str, intent: super::intent::Intent) -> bool {
        self.entries
            .get(&format!("{source}|{}|{query}", intent.code()))
            .is_some_and(|entry| valid(entry, now()) && has_native_evidence(query, &entry.hits))
    }

    pub fn counts(&self) -> (usize, usize) {
        (
            self.entries.len(),
            self.entries.values().filter(|e| valid(e, now())).count(),
        )
    }
}

fn valid(entry: &Entry, time: u64) -> bool {
    entry.quality == AnswerClass::Answered
        && !entry.hits.is_empty()
        && time
            .checked_sub(entry.written)
            .is_some_and(|age| age < entry.ttl)
}

fn has_native_evidence(query: &str, hits: &[super::engines::Hit]) -> bool {
    super::locale::evidence_language(query).is_none_or(|lang| {
        hits.iter()
            .any(|hit| super::locale::native(lang, &hit.title, &hit.snippet))
    })
}

/// The exact query is part of the key: scoped, excluded and quoted queries
/// never borrow a different question's answer. TTL follows the source class.
pub(super) async fn run(
    cache: &std::sync::Mutex<LaneCache>,
    source: String,
    query: String,
    intent: super::intent::Intent,
    future: TaskFut<'_>,
) -> (String, EngineResult) {
    let key = format!("{source}|{}|{query}", intent.code());
    {
        let guard = cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(entry) = guard
            .entries
            .get(&key)
            .filter(|e| valid(e, now()) && has_native_evidence(&query, &e.hits))
        {
            return (
                source,
                Ok(LaneAnswer {
                    hits: entry.hits.clone(),
                    items: entry.hits.len(),
                    ms: 0,
                    egress: "cache".into(),
                    engine_lane: false,
                    instant: None,
                    class: entry.quality,
                    requests: 0,
                    cache_age: Some(Duration::from_secs(now().saturating_sub(entry.written))),
                }),
            );
        }
    }
    let (label, answer) = future.await;
    if let Ok(answer) = &answer
        && answer.class == AnswerClass::Answered
        && !answer.hits.is_empty()
        && has_native_evidence(&query, &answer.hits)
    {
        let ttl = if super::news::wants_news(&query, intent) {
            120
        } else if matches!(
            source.as_str(),
            "docs" | "registry" | "crates" | "npm" | "pypi"
        ) {
            86_400
        } else {
            super::cache_ttl(intent, &query).as_secs()
        };
        cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entries
            .insert(
                key,
                Entry {
                    written: now(),
                    ttl,
                    quality: answer.class,
                    hits: answer.hits.clone(),
                },
            );
    }
    (label, answer)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn only_answered_entries_inside_their_own_ttl_are_replayable() {
        let mut entry = Entry {
            written: 100,
            ttl: 120,
            quality: AnswerClass::Answered,
            hits: vec![super::super::engines::Hit {
                title: "Example".into(),
                url: "https://example.com/a".into(),
                snippet: "Evidence".into(),
                rank: 1,
                published: None,
            }],
        };
        assert!(valid(&entry, 219));
        assert!(!valid(&entry, 220));
        assert!(!valid(&entry, 99));
        for quality in [AnswerClass::Thin, AnswerClass::Junk, AnswerClass::Empty] {
            entry.quality = quality;
            assert!(!valid(&entry, 101));
        }
        entry.quality = AnswerClass::Answered;
        entry.hits.clear();
        assert!(!valid(&entry, 101));
        entry.written = now();
        entry.hits.push(super::super::engines::Hit {
            title: "English weather".into(),
            url: "https://example.com/a".into(),
            snippet: "English evidence".into(),
            rank: 0,
            published: None,
        });
        let query = "日本語の天気";
        let intent = super::super::intent::Intent::Web;
        let cache = std::sync::Mutex::new(LaneCache::default());
        cache
            .lock()
            .unwrap()
            .entries
            .insert(format!("bing|{}|{query}", intent.code()), entry);
        assert!(!cache.lock().unwrap().contains("bing", query, intent));
        let fresh: TaskFut<'_> = Box::pin(async {
            (
                "bing".into(),
                Err(("fresh-request".into(), "direct".into(), true)),
            )
        });
        let (_, answer) = run(&cache, "bing".into(), query.into(), intent, fresh).await;
        assert_eq!(
            answer.unwrap_err().0,
            "fresh-request",
            "a foreign cached answer must not prevent a fresh native query"
        );
    }
}
