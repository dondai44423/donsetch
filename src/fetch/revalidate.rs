//! Bounded, request-context-specific HTTP response revalidation.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

pub struct CacheEntry {
    pub body: Arc<[u8]>,
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    pub fresh_until: Option<Instant>,
    received: Instant,
    initial_age: Duration,
}

pub enum CacheCheck {
    Fresh(Vec<u8>, u16, Vec<(String, String)>),
    /// Validators and the immutable representation they identify.
    Revalidate(Vec<(String, String)>, Arc<CacheEntry>),
    None,
}

pub struct RevalidationCache {
    map: HashMap<String, Arc<CacheEntry>>,
    queue: std::collections::VecDeque<String>,
    total_bytes: usize,
}

const MAX_ENTRIES: usize = 512;
const MAX_BODY: usize = 8 << 20;
const MAX_TOTAL_BYTES: usize = 64 << 20;
const MAX_HEADER_BYTES: usize = 64 << 10;
const MAX_HEADER_FIELDS: usize = 256;

impl RevalidationCache {
    pub fn new() -> Self {
        Self {
            map: HashMap::new(),
            queue: std::collections::VecDeque::new(),
            total_bytes: 0,
        }
    }

    pub fn check(&self, key: &str) -> CacheCheck {
        let Some(entry) = self.map.get(key) else {
            return CacheCheck::None;
        };
        if entry
            .fresh_until
            .is_some_and(|until| Instant::now() < until)
        {
            return CacheCheck::Fresh(entry.body.to_vec(), entry.status, aged_headers(entry));
        }
        let mut conditional = Vec::new();
        if let Some(tag) = &entry.etag {
            conditional.push(("if-none-match".into(), tag.clone()));
        }
        if let Some(date) = &entry.last_modified {
            conditional.push(("if-modified-since".into(), date.clone()));
        }
        if conditional.is_empty() {
            CacheCheck::None
        } else {
            CacheCheck::Revalidate(conditional, entry.clone())
        }
    }

    /// Merge only the representation that generated this request's validators.
    /// A concurrent replacement remains authoritative in the cache.
    #[allow(clippy::type_complexity)]
    pub fn revalidated(
        &mut self,
        key: &str,
        snapshot: &Arc<CacheEntry>,
        headers: &[(String, String)],
        response_delay: Duration,
    ) -> Result<(Vec<u8>, u16, Vec<(String, String)>), String> {
        let headers = stored_headers(headers);
        let tags: Vec<_> = headers
            .iter()
            .filter(|(name, _)| name.eq_ignore_ascii_case("etag"))
            .collect();
        // A 304 answering a Last-Modified conditional may carry an ETag
        // the snapshot never had (example.com behind Cloudflare: the 200
        // ships no ETag, the 304 does). There is no stored validator to
        // compare against, and RFC 9111 §4.3.4 makes the field metadata
        // the merge updates; only when the snapshot actually carried an
        // entity-tag does the comparison (and its rejection) apply.
        // Equality ignores the weak prefix (RFC 9110 §13.1.2), and
        // the strict CAS adds one rule on top: a weak -> strong upgrade
        // is rejected. The stored bytes were only weakly coupled to
        // their tag, and the origin's stronger claim is not evidence
        // this client holds (stealth_v3_304_changed_validators_fail_
        // without_merging_a_body pins the full policy). A different
        // opaque tag, an unparseable tag, or an ambiguous multi-tag
        // answer is a rejected 304 too; the caller's bounded recovery
        // refetches each rejected shape unconditionally.
        if let Some((_, tag)) = tags.first()
            && let Some(old) = snapshot.etag.as_deref()
            && (tags.len() != 1
                || !valid_etag(tag)
                || !valid_etag(old)
                || opaque_tag(tag) != opaque_tag(old)
                || (old.starts_with("W/") && !tag.trim().starts_with("W/")))
        {
            return Err("304 ETag does not identify the requested representation".into());
        }
        if tags.is_empty() {
            let dates: Vec<_> = headers
                .iter()
                .filter(|(name, _)| name.eq_ignore_ascii_case("last-modified"))
                .collect();
            if let Some((_, date)) = dates.first()
                && (dates.len() != 1
                    || httpdate::parse_http_date(date).ok().is_none_or(|date| {
                        snapshot
                            .last_modified
                            .as_deref()
                            .and_then(|old| httpdate::parse_http_date(old).ok())
                            != Some(date)
                    }))
            {
                return Err(
                    "304 Last-Modified does not identify the requested representation".into(),
                );
            }
        }
        // Connection-specific fields cannot become stored representation metadata.
        let mut merged = snapshot.headers.clone();
        // Bodies have already been decoded. These fields must keep describing
        // that stored representation (RFC 9111 sections 3.1 and 3.2).
        let updated: Vec<_> = headers
            .iter()
            .filter(|(name, _)| {
                !name.eq_ignore_ascii_case("content-length")
                    && !name.eq_ignore_ascii_case("content-encoding")
            })
            .collect();
        merged.retain(|(name, _)| {
            !updated
                .iter()
                .any(|(new, _)| new.eq_ignore_ascii_case(name))
                && !name.eq_ignore_ascii_case("age")
                && !name.eq_ignore_ascii_case("date")
        });
        merged.extend(updated.into_iter().cloned());
        let now = SystemTime::now();
        add_missing_date(&mut merged, now);
        let refreshed = entry(
            snapshot.body.clone(),
            snapshot.status,
            &merged,
            response_delay,
            now,
        );
        if self
            .map
            .get(key)
            .is_some_and(|current| Arc::ptr_eq(current, snapshot))
        {
            if let Some(refreshed) = &refreshed {
                self.insert(key, refreshed.clone());
            } else {
                self.remove(key);
            }
        }
        let response_headers = refreshed
            .as_ref()
            .map_or(merged, |entry| aged_headers(entry));
        Ok((snapshot.body.to_vec(), snapshot.status, response_headers))
    }

    pub fn store(&mut self, key: &str, status: u16, headers: &[(String, String)], body: &[u8]) {
        self.store_with_delay(key, status, headers, body, Duration::ZERO);
    }

    pub fn store_with_delay(
        &mut self,
        key: &str,
        status: u16,
        headers: &[(String, String)],
        body: &[u8],
        response_delay: Duration,
    ) {
        if status != 200 {
            self.remove(key);
            return;
        }
        if body.len() > MAX_BODY {
            self.remove(key);
            return;
        }
        if let Some(entry) = entry(
            Arc::from(body),
            status,
            headers,
            response_delay,
            SystemTime::now(),
        ) {
            self.insert(key, entry);
        } else {
            self.remove(key);
        }
    }

    pub(crate) fn remove(&mut self, key: &str) {
        if let Some(old) = self.map.remove(key) {
            self.total_bytes -= old.body.len();
            self.queue.retain(|queued| queued != key);
        }
    }

    fn insert(&mut self, key: &str, entry: Arc<CacheEntry>) {
        let replacing = self.map.get(key).map_or(0, |old| old.body.len());
        let mut resident = self.total_bytes - replacing;
        while (self.map.len() >= MAX_ENTRIES && !self.map.contains_key(key))
            || resident + entry.body.len() > MAX_TOTAL_BYTES
        {
            let Some(oldest) = self.queue.pop_front() else {
                break;
            };
            if oldest == key {
                self.map.remove(&oldest);
                continue;
            }
            if let Some(old) = self.map.remove(&oldest) {
                resident -= old.body.len();
            }
        }
        if !self.map.contains_key(key) {
            self.queue.push_back(key.to_owned());
        }
        self.total_bytes = resident + entry.body.len();
        self.map.insert(key.to_owned(), entry);
    }
}

impl Default for RevalidationCache {
    fn default() -> Self {
        Self::new()
    }
}

fn hop_header(name: &str) -> bool {
    [
        "connection",
        "keep-alive",
        "proxy-authenticate",
        "proxy-authorization",
        "te",
        "trailer",
        "transfer-encoding",
        "upgrade",
    ]
    .iter()
    .any(|hop| name.eq_ignore_ascii_case(hop))
}

fn stored_headers(headers: &[(String, String)]) -> Vec<(String, String)> {
    let connection: Vec<_> = headers
        .iter()
        .filter(|(name, _)| name.eq_ignore_ascii_case("connection"))
        .flat_map(|(_, value)| value.split(','))
        .map(str::trim)
        .collect();
    headers
        .iter()
        .filter(|(name, _)| {
            !hop_header(name)
                && !connection
                    .iter()
                    .any(|field| name.eq_ignore_ascii_case(field))
        })
        .cloned()
        .collect()
}

/// The opaque entity-tag: the quoted string with any weak prefix
/// removed (RFC 9110 §8.8.3). If-None-Match matching compares only the
/// opaque tag; the W/ prefix never participates.
fn opaque_tag(tag: &str) -> &str {
    tag.trim().strip_prefix("W/").unwrap_or(tag.trim())
}

fn valid_etag(tag: &str) -> bool {
    let tag = opaque_tag(tag).as_bytes();
    tag.len() >= 2
        && tag[0] == b'"'
        && tag[tag.len() - 1] == b'"'
        && tag[1..tag.len() - 1]
            .iter()
            .all(|&byte| byte == 0x21 || (0x23..=0x7e).contains(&byte) || byte >= 0x80)
}

fn add_missing_date(headers: &mut Vec<(String, String)>, now: SystemTime) {
    if !headers
        .iter()
        .any(|(name, _)| name.eq_ignore_ascii_case("date"))
    {
        headers.push(("date".into(), httpdate::fmt_http_date(now)));
    }
}

fn aged_headers(entry: &CacheEntry) -> Vec<(String, String)> {
    let mut headers = entry.headers.clone();
    headers.retain(|(name, _)| !name.eq_ignore_ascii_case("age"));
    let age = entry
        .initial_age
        .saturating_add(entry.received.elapsed())
        .as_secs()
        .min(1 << 31);
    headers.push(("age".into(), age.to_string()));
    headers
}

fn entry(
    body: Arc<[u8]>,
    status: u16,
    headers: &[(String, String)],
    response_delay: Duration,
    now: SystemTime,
) -> Option<Arc<CacheEntry>> {
    if headers.len() > MAX_HEADER_FIELDS
        || headers.iter().fold(0usize, |size, (name, value)| {
            size.saturating_add(name.len()).saturating_add(value.len())
        }) > MAX_HEADER_BYTES
    {
        return None;
    }
    let mut headers = stored_headers(headers);
    let unkeyed_vary = headers
        .iter()
        .filter(|(name, _)| name.eq_ignore_ascii_case("vary"))
        .flat_map(|(_, value)| value.split(','))
        .map(str::trim)
        .filter(|field| !field.is_empty())
        .any(|field| {
            field == "*"
                || field.eq_ignore_ascii_case("if-none-match")
                || field.eq_ignore_ascii_case("if-modified-since")
                || !field
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&byte))
        });
    let policy = cache_policy(&headers);
    if unkeyed_vary || policy.no_store {
        return None;
    }
    let get = |name: &str| {
        let mut values = headers
            .iter()
            .filter(|(field, _)| field.eq_ignore_ascii_case(name));
        let value = values.next()?.1.trim();
        values.next().is_none().then_some(value)
    };
    let etag = get("etag").filter(|tag| valid_etag(tag)).map(str::to_owned);
    let last_modified = get("last-modified")
        .filter(|date| httpdate::parse_http_date(date).is_ok())
        .map(str::to_owned);
    let age = if !headers
        .iter()
        .any(|(name, _)| name.eq_ignore_ascii_case("age"))
    {
        Some(0)
    } else {
        get("age").and_then(delta_seconds)
    };
    let date = if !headers
        .iter()
        .any(|(name, _)| name.eq_ignore_ascii_case("date"))
    {
        Some(now)
    } else {
        get("date").and_then(|value| httpdate::parse_http_date(value).ok())
    };
    let initial_age = match (age, date) {
        (Some(age), Some(date)) => Duration::from_secs(age)
            .saturating_add(response_delay)
            .max(now.duration_since(date).unwrap_or_default()),
        _ => Duration::MAX,
    };
    let received = Instant::now();
    let fresh_until = if policy.validate {
        None
    } else {
        policy.max_age.map(|age| {
            received
                + Duration::from_secs(age)
                    .saturating_sub(initial_age)
                    .min(Duration::from_secs(3600))
        })
    };
    if etag.is_none()
        && last_modified.is_none()
        && fresh_until.is_none_or(|until| until <= received)
    {
        return None;
    }
    add_missing_date(&mut headers, now);
    Some(Arc::new(CacheEntry {
        body,
        status,
        headers,
        etag,
        last_modified,
        fresh_until,
        received,
        initial_age,
    }))
}

#[derive(Default)]
struct CachePolicy {
    no_store: bool,
    validate: bool,
    max_age: Option<u64>,
}

fn cache_policy(headers: &[(String, String)]) -> CachePolicy {
    let mut policy = CachePolicy::default();
    let mut max_age_seen = false;
    for (_, value) in headers
        .iter()
        .filter(|(name, _)| name.eq_ignore_ascii_case("cache-control"))
    {
        let mut quoted = false;
        let mut escaped = false;
        let mut start = 0;
        for (end, byte) in value
            .bytes()
            .enumerate()
            .chain(std::iter::once((value.len(), b',')))
        {
            if escaped {
                escaped = false;
                continue;
            }
            if quoted && byte == b'\\' {
                escaped = true;
                continue;
            }
            if byte == b'"' {
                quoted = !quoted;
                continue;
            }
            if byte != b',' || quoted {
                continue;
            }
            let directive = value[start..end].trim();
            start = end + 1;
            let (name, argument) = directive
                .split_once('=')
                .map_or((directive, None), |(name, value)| {
                    (name.trim(), Some(value.trim()))
                });
            if name.eq_ignore_ascii_case("no-store") || name.eq_ignore_ascii_case("private") {
                policy.no_store = true;
            } else if name.eq_ignore_ascii_case("no-cache") {
                policy.validate = true;
            } else if name.eq_ignore_ascii_case("max-age") {
                if max_age_seen {
                    policy.validate = true;
                }
                max_age_seen = true;
                policy.max_age = argument.and_then(|value| {
                    let value = if value.starts_with('"') {
                        value.strip_prefix('"')?.strip_suffix('"')?
                    } else {
                        value
                    };
                    delta_seconds(value)
                });
                if policy.max_age.is_none() {
                    policy.validate = true;
                }
            }
        }
        if quoted || escaped {
            policy.validate = true;
            let name = value[start..].split('=').next().unwrap_or_default().trim();
            if name.eq_ignore_ascii_case("no-store") || name.eq_ignore_ascii_case("private") {
                policy.no_store = true;
            }
        }
    }
    policy
}

fn delta_seconds(value: &str) -> Option<u64> {
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    Some(value.bytes().fold(0u64, |total, digit| {
        total
            .saturating_mul(10)
            .saturating_add(u64::from(digit - b'0'))
    }))
}

#[cfg(test)]
mod audit_tests {
    use super::*;

    #[test]
    fn stealth_v3_cache_metadata_bounds_evict_without_hiding_the_current_body() {
        for headers in [
            vec![
                ("cache-control".into(), "max-age=600".into()),
                ("x-large".into(), "x".repeat(MAX_HEADER_BYTES)),
            ],
            (0..MAX_HEADER_FIELDS)
                .map(|i| (format!("x-{i}"), "v".into()))
                .chain(std::iter::once((
                    "cache-control".into(),
                    "max-age=600".into(),
                )))
                .collect(),
        ] {
            let mut cache = RevalidationCache::new();
            cache.store(
                "owned",
                200,
                &[("etag".into(), "\"old\"".into())],
                b"old body",
            );
            cache.store("owned", 200, &headers, b"current body");
            assert!(matches!(cache.check("owned"), CacheCheck::None));
            assert_eq!(cache.total_bytes, 0);
            assert!(cache.queue.is_empty());
        }
        let mut cache = RevalidationCache::new();
        cache.store(
            "owned",
            200,
            &[
                ("cache-control".into(), "max-age=600".into()),
                ("x-large".into(), "x".repeat(MAX_HEADER_BYTES / 2)),
            ],
            b"body",
        );
        assert!(
            matches!(cache.check("owned"), CacheCheck::Fresh(..)),
            "ordinary bounded metadata remains cacheable"
        );
    }

    #[test]
    fn stealth_v3_304_preserves_decoded_body_metadata_and_discards_connection_fields() {
        let mut cache = RevalidationCache::new();
        cache.store(
            "owned",
            200,
            &[
                ("etag".into(), "\"v1\"".into()),
                ("content-length".into(), "27".into()),
                ("content-encoding".into(), "gzip".into()),
                ("connection".into(), "x-private-hop".into()),
                ("x-private-hop".into(), "original hop".into()),
            ],
            b"decoded body",
        );
        let CacheCheck::Revalidate(_, snapshot) = cache.check("owned") else {
            panic!("validator snapshot");
        };
        let (body, _, headers) = cache
            .revalidated(
                "owned",
                &snapshot,
                &[
                    ("etag".into(), "\"v1\"".into()),
                    ("cache-control".into(), "max-age=600".into()),
                    ("content-length".into(), "999".into()),
                    ("content-encoding".into(), "br".into()),
                    ("connection".into(), "x-private-hop".into()),
                    ("x-private-hop".into(), "new hop".into()),
                ],
                Duration::ZERO,
            )
            .unwrap();
        assert_eq!(body, b"decoded body");
        assert!(
            headers
                .iter()
                .any(|(name, value)| name == "content-length" && value == "27")
        );
        assert!(
            headers
                .iter()
                .any(|(name, value)| name == "content-encoding" && value == "gzip")
        );
        assert!(
            !headers
                .iter()
                .any(|(name, _)| name == "connection" || name == "x-private-hop")
        );
        assert!(matches!(cache.check("owned"), CacheCheck::Fresh(..)));
    }

    #[test]
    fn stealth_v3_304_snapshot_survives_eviction_without_resurrecting_the_cache() {
        for policy in ["no-store", "private", "max-age=600"] {
            let mut cache = RevalidationCache::new();
            cache.store(
                "owned",
                200,
                &[("etag".into(), "\"v1\"".into())],
                b"snapshot body",
            );
            let CacheCheck::Revalidate(_, snapshot) = cache.check("owned") else {
                panic!("validator snapshot");
            };
            if policy == "max-age=600" {
                cache.remove("owned");
            }
            let (body, status, _) = cache
                .revalidated(
                    "owned",
                    &snapshot,
                    &[
                        ("etag".into(), "\"v1\"".into()),
                        ("cache-control".into(), policy.into()),
                    ],
                    Duration::ZERO,
                )
                .unwrap();
            assert_eq!(body, b"snapshot body");
            assert_eq!(status, 200);
            assert!(
                matches!(cache.check("owned"), CacheCheck::None),
                "policy/eviction {policy}"
            );
            assert_eq!(cache.total_bytes, 0);
            assert!(cache.queue.is_empty());
        }
    }

    #[test]
    fn stealth_v3_connection_nominated_freshness_cannot_authorize_reuse() {
        let mut cache = RevalidationCache::new();
        cache.store(
            "owned",
            200,
            &[
                ("connection".into(), "Cache-Control, ETag".into()),
                ("cache-control".into(), "max-age=600".into()),
                ("etag".into(), "\"hop-only\"".into()),
            ],
            b"hop only body",
        );
        assert!(matches!(cache.check("owned"), CacheCheck::None));
        assert_eq!(cache.total_bytes, 0);
    }

    #[test]
    fn stealth_v3_uncacheable_policy_evicts_the_previous_representation() {
        for directive in [
            "no-store",
            "private",
            "no-cache",
            "no-store=\"broken",
            "private=\"broken",
        ] {
            let mut cache = RevalidationCache::new();
            cache.store(
                "owned",
                200,
                &[("etag".into(), "\"old\"".into())],
                b"old body",
            );
            cache.store(
                "owned",
                200,
                &[
                    ("cache-control".into(), "max-age=600".into()),
                    ("Cache-Control".into(), directive.into()),
                ],
                b"new uncacheable body",
            );
            assert!(
                matches!(cache.check("owned"), CacheCheck::None),
                "new policy {directive} must not leave an older or fresh body reusable"
            );
            assert_eq!(cache.total_bytes, 0);
            assert!(cache.queue.is_empty());
        }
    }

    #[test]
    fn stealth_v3_cache_directives_are_tokens_across_all_header_fields() {
        for directive in [
            "max-age=60, max-age=600",
            "max-age=600, max-age=60",
            "max-age=+600",
            "max-age=600, max-age=bad",
            "max-age=\"600",
            "max-age=600, no-cache",
        ] {
            let mut cache = RevalidationCache::new();
            cache.store(
                "owned",
                200,
                &[
                    ("etag".into(), "\"v1\"".into()),
                    ("cache-control".into(), directive.into()),
                ],
                b"body",
            );
            assert!(
                matches!(cache.check("owned"), CacheCheck::Revalidate(..)),
                "invalid or ambiguous freshness must validate: {directive}"
            );
        }
        for extension in [
            "ext=\"private\"",
            "ext=\"no-store, no-cache\"",
            "private-extension=value",
            "ext=\"quote\\\" no-cache\"",
        ] {
            let mut cache = RevalidationCache::new();
            cache.store(
                "owned",
                200,
                &[
                    ("cache-control".into(), extension.into()),
                    ("Cache-Control".into(), "max-age=600".into()),
                ],
                b"body",
            );
            assert!(
                matches!(cache.check("owned"), CacheCheck::Fresh(..)),
                "quoted extension values are not directives: {extension}"
            );
        }
    }

    #[test]
    fn stealth_v3_received_age_and_date_do_not_restart_an_expired_fresh_window() {
        for age_headers in [
            vec![("age".into(), "601".into())],
            vec![("date".into(), "Sun, 06 Nov 1994 08:49:37 GMT".into())],
            vec![("age".into(), "invalid".into())],
            vec![("age".into(), "-1".into())],
            vec![("age".into(), "99999999999999999999999999999".into())],
            vec![("age".into(), "20".into()), ("Age".into(), "0".into())],
            vec![("date".into(), "Wed, 29 Feb 2023 00:00:00 GMT".into())],
            vec![("date".into(), "Sun, 06 Nov 1994 08:49:37 UTC extra".into())],
        ] {
            let mut headers = vec![
                ("cache-control".into(), "max-age=600".into()),
                ("etag".into(), "\"v1\"".into()),
            ];
            headers.extend(age_headers);
            let mut cache = RevalidationCache::new();
            cache.store("owned", 200, &headers, b"body");
            assert!(
                matches!(cache.check("owned"), CacheCheck::Revalidate(..)),
                "expired or invalid age must not become fresh: {headers:?}"
            );
        }
    }

    #[test]
    fn stealth_v3_vary_checks_every_field_and_rejects_unkeyed_conditionals() {
        for vary in [
            "Cookie, *",
            "Accept, bad field",
            "If-None-Match",
            "if-modified-since",
            "Accept, \u{00e9}",
        ] {
            let mut cache = RevalidationCache::new();
            cache.store(
                "owned",
                200,
                &[
                    ("cache-control".into(), "max-age=600".into()),
                    ("vary".into(), "Accept".into()),
                    ("Vary".into(), vary.into()),
                ],
                b"must not cache",
            );
            assert!(
                matches!(cache.check("owned"), CacheCheck::None),
                "invalid or unkeyed Vary: {vary}"
            );
        }
        let mut cache = RevalidationCache::new();
        cache.store(
            "owned",
            200,
            &[
                ("cache-control".into(), "max-age=600".into()),
                ("vary".into(), "Cookie, Referer".into()),
            ],
            b"keyed representation",
        );
        assert!(matches!(cache.check("owned"), CacheCheck::Fresh(..)));
        cache.store(
            "owned",
            200,
            &[
                ("cache-control".into(), "max-age=600".into()),
                ("vary".into(), "*".into()),
            ],
            b"changed policy",
        );
        assert!(
            matches!(cache.check("owned"), CacheCheck::None),
            "a new uncacheable response must evict the old representation"
        );
        assert_eq!(cache.total_bytes, 0);
        assert!(cache.queue.is_empty());
    }

    #[test]
    fn vary_star_is_never_stored() {
        let mut c = RevalidationCache::new();
        c.store(
            "https://x.test/a",
            200,
            &[
                ("etag".into(), "\"v1\"".into()),
                ("vary".into(), "*".into()),
            ],
            b"body",
        );
        assert!(
            matches!(c.check("https://x.test/a"), CacheCheck::None),
            "Vary: * must not be stored (E12)"
        );
    }

    // RFC 9111 §5.2.2.4: no-cache is stored but must NEVER be served
    // without validation. "no-cache, max-age=3600" used to be served
    // fresh for an hour, defying the origin's explicit instruction.
    #[test]
    fn no_cache_stores_but_never_serves_fresh() {
        let mut c = RevalidationCache::new();
        c.store(
            "https://x.test/nc",
            200,
            &[
                ("etag".into(), "\"v1\"".into()),
                ("cache-control".into(), "no-cache, max-age=3600".into()),
            ],
            b"body",
        );
        assert!(
            matches!(c.check("https://x.test/nc"), CacheCheck::Revalidate(..)),
            "no-cache must send the conditional on every hit"
        );
    }

    // 512 entries of 8 MiB each was 4 GiB resident. The byte budget
    // evicts oldest-first until the new body fits.
    #[test]
    fn resident_bytes_stay_under_the_budget() {
        let mut c = RevalidationCache::new();
        let big = vec![b'x'; MAX_BODY];
        for i in 0..16 {
            c.store(
                &format!("https://x.test/big/{i}"),
                200,
                &[("etag".into(), format!("\"e{i}\""))],
                &big,
            );
            let sum: usize = c.map.values().map(|e| e.body.len()).sum();
            assert!(sum <= MAX_TOTAL_BYTES, "after {i}: {sum} resident");
            assert_eq!(c.total_bytes, sum, "the counter tracks the map");
        }
        assert_eq!(c.map.len(), MAX_TOTAL_BYTES / MAX_BODY);
        assert!(
            !c.map.contains_key("https://x.test/big/0"),
            "oldest evicted first"
        );
        assert!(c.map.contains_key("https://x.test/big/15"), "newest kept");
        // A re-store of an existing key swaps its bytes, no double count.
        c.store(
            "https://x.test/big/15",
            200,
            &[("etag".into(), "\"e15b\"".into())],
            b"small",
        );
        let sum: usize = c.map.values().map(|e| e.body.len()).sum();
        assert_eq!(c.total_bytes, sum);
        assert_eq!(c.map.len(), MAX_TOTAL_BYTES / MAX_BODY);
    }

    #[test]
    fn eviction_is_fifo() {
        let mut c = RevalidationCache::new();
        for i in 0..MAX_ENTRIES {
            c.store(
                &format!("https://x.test/{i}"),
                200,
                &[("etag".into(), format!("\"e{i}\""))],
                b"b",
            );
        }
        // Refresh entry 0 to mark it hot, then overflow by one: FIFO
        // evicts the OLDEST INSERT (entry 0), not an arbitrary victim.
        c.store(
            "https://x.test/0",
            200,
            &[("etag".into(), "\"hot\"".into())],
            b"b",
        );
        c.store(
            &format!("https://x.test/{MAX_ENTRIES}"),
            200,
            &[("etag".into(), "\"new\"".into())],
            b"b",
        );
        assert_eq!(c.map.len(), MAX_ENTRIES, "capped");
        assert!(
            !c.map.contains_key("https://x.test/0"),
            "FIFO evicts the oldest entry"
        );
        assert!(
            c.map.contains_key("https://x.test/1"),
            "second-oldest survives"
        );
        assert!(
            c.map.contains_key(&format!("https://x.test/{MAX_ENTRIES}")),
            "newest survives"
        );
    }

    // example.com behind Cloudflare: the 200 has last-modified but no
    // ETag; the 304 (answering our If-Modified-Since) carries the ETag.
    // RFC 9111 4.3.4: the merge updates metadata; with no stored
    // entity-tag there is nothing to compare, and the fetch must not
    // fail. Once merged, the stored ETag drives the ordinary CAS.
    #[test]
    fn stealth_v3_304_etag_over_last_modified_validation_updates_metadata() {
        let mut cache = RevalidationCache::new();
        cache.store(
            "owned",
            200,
            &[
                (
                    "last-modified".into(),
                    "Sun, 04 Oct 2026 20:44:03 GMT".into(),
                ),
                ("age".into(), "400".into()),
            ],
            b"lm body",
        );
        let CacheCheck::Revalidate(conditional, snapshot) = cache.check("owned") else {
            panic!("last-modified snapshot");
        };
        assert_eq!(conditional.len(), 1);
        assert_eq!(conditional[0].0, "if-modified-since");
        let (body, status, headers) = cache
            .revalidated(
                "owned",
                &snapshot,
                &[
                    ("etag".into(), "\"fixture-lm-etag-1\"".into()),
                    (
                        "last-modified".into(),
                        "Sun, 04 Oct 2026 20:44:03 GMT".into(),
                    ),
                ],
                Duration::ZERO,
            )
            .unwrap();
        assert_eq!(body, b"lm body");
        assert_eq!(status, 200);
        assert!(
            headers
                .iter()
                .any(|(n, v)| n == "etag" && v == "\"fixture-lm-etag-1\"")
        );
        // The stored ETag now exists: the next hit compares, and a
        // mismatched 304 still rejects (the qualified CAS).
        let CacheCheck::Revalidate(conditional, snapshot) = cache.check("owned") else {
            panic!("etag snapshot after merge");
        };
        assert!(conditional.iter().any(|(n, _)| n == "if-none-match"));
        assert!(
            cache
                .revalidated(
                    "owned",
                    &snapshot,
                    &[("etag".into(), "\"different\"".into())],
                    Duration::ZERO,
                )
                .is_err(),
            "a mismatched 304 must still be rejected"
        );
        cache
            .revalidated(
                "owned",
                &snapshot,
                &[("etag".into(), "\"fixture-lm-etag-1\"".into())],
                Duration::ZERO,
            )
            .unwrap();
    }

    // The strict CAS as shipped: equality ignores the weak prefix
    // (RFC 9110 §13.1.2), but a weak -> strong upgrade is rejected on
    // top of that (the stored bytes were only weakly coupled to their
    // tag), and so are opaque mismatches and ambiguous multi-tag
    // answers. The caller's bounded recovery refetches every rejected
    // shape. The integration suite pins the same policy through the
    // full client (stealth_v3_304_changed_validators_fail_without_
    // merging_a_body).
    #[test]
    fn stealth_v3_304_strict_upgrade_policy_is_the_cas() {
        for (stored, returned) in [
            ("\"x\"", "\"x\""),     // identical
            ("\"x\"", "W/\"x\""),   // weakening: merge, adopt the weak tag
            ("W/\"x\"", "W/\"x\""), // identical weak
        ] {
            let mut cache = RevalidationCache::new();
            cache.store("owned", 200, &[("etag".into(), stored.into())], b"body");
            let CacheCheck::Revalidate(_, snapshot) = cache.check("owned") else {
                panic!("validator snapshot for {stored}");
            };
            let merged = cache.revalidated(
                "owned",
                &snapshot,
                &[
                    ("etag".into(), returned.into()),
                    ("cache-control".into(), "max-age=600".into()),
                ],
                Duration::ZERO,
            );
            assert!(
                merged.is_ok(),
                "{stored} vs {returned} must merge: {merged:?}"
            );
            assert_eq!(merged.unwrap().0.as_slice(), b"body");
        }
        for (stored, headers) in [
            ("W/\"x\"", vec![("etag".to_string(), "\"x\"".to_string())]),
            ("\"x\"", vec![("etag".to_string(), "\"y\"".to_string())]),
            (
                "\"x\"",
                vec![
                    ("etag".to_string(), "\"x\"".to_string()),
                    ("etag".to_string(), "\"x\"".to_string()),
                ],
            ),
        ] {
            let mut cache = RevalidationCache::new();
            cache.store("owned", 200, &[("etag".into(), stored.into())], b"body");
            let CacheCheck::Revalidate(_, snapshot) = cache.check("owned") else {
                panic!("validator snapshot for {stored}");
            };
            assert!(
                cache
                    .revalidated("owned", &snapshot, &headers, Duration::ZERO)
                    .is_err(),
                "{stored} with {headers:?} must be rejected"
            );
        }
    }
}
