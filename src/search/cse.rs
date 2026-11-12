//! Google's public search-element request contract, as used by Blackle.
//!
//! Protocol reference: SearXNG's google_cse engine (AGPL-3.0-or-later),
//! https://developers.google.com/custom-search/docs/element. The bootstrap's
//! own JSON provides its token and library version; no JavaScript is executed.
//! Both requests use our own client and the same egress. Refusals stand.

use super::{
    engines::Hit,
    tasks::{EngineContext, EngineResult, LaneAnswer},
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const SOURCE: &str = "google-cse";
const CX: &str = "partner-pub-8993703457585266:4862972284";
const TOKEN_TTL: u64 = 3600;
const BODY_CAP: usize = 3 << 20;

#[derive(Clone, Serialize, Deserialize)]
struct Token {
    value: String,
    version: String,
    experiments: String,
    written: u64,
}

impl Token {
    fn fresh(&self, time: u64) -> bool {
        !self.value.is_empty()
            && time
                .checked_sub(self.written)
                .is_some_and(|age| age < TOKEN_TTL)
    }
}

#[derive(Default)]
pub(super) struct Client {
    tokens: Mutex<HashMap<String, Token>>,
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
fn path() -> std::path::PathBuf {
    crate::paths::cache_dir().join("search-cse.json")
}

impl Client {
    pub(super) fn request_cost(&self, proxy: Option<&crate::egress::proxy::Proxy>) -> usize {
        let Ok(proxy) = request_proxy(proxy) else {
            return 2;
        };
        match self.get(&token_key(proxy.as_ref())) {
            // Planning includes pacing and a pending hedge. A nearly expired
            // token must not turn one planned exchange into two at dispatch.
            Some(token) if token.fresh(now().saturating_add(10)) => 1,
            _ => 2,
        }
    }

    pub fn load() -> Self {
        let client = Self::default();
        if crate::config::cfg().state.no_disk_state {
            return client;
        }
        let path = path();
        if std::fs::metadata(&path).is_ok_and(|m| m.len() < 128 << 10)
            && let Ok(bytes) = std::fs::read(path)
            && let Ok(mut tokens) = serde_json::from_slice::<HashMap<String, Token>>(&bytes)
        {
            tokens.retain(|_, token| token.fresh(now()));
            if tokens.len() <= 64 {
                *client
                    .tokens
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = tokens;
            }
        }
        client
    }
    fn get(&self, lane: &str) -> Option<Token> {
        self.tokens
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(lane)
            .filter(|t| t.fresh(now()))
            .cloned()
    }
    fn put(&self, lane: &str, token: Option<Token>) {
        let mut tokens = self
            .tokens
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        tokens.retain(|_, token| token.fresh(now()));
        if let Some(token) = token {
            if tokens.len() >= 64 {
                tokens.clear();
            }
            tokens.insert(lane.to_string(), token);
        } else {
            tokens.remove(lane);
        }
        if crate::config::cfg().state.no_disk_state {
            return;
        }
        if let Ok(bytes) = serde_json::to_vec(&*tokens) {
            // Cache metadata only. A partial/interrupted write is ignored by
            // load; write_private also works when replacing a file on Windows.
            let _ = crate::config::write_private(&path(), &bytes);
        }
    }
}

fn request_proxy(
    explicit: Option<&crate::egress::proxy::Proxy>,
) -> Result<Option<crate::egress::proxy::Proxy>, String> {
    if let Some(proxy) = explicit {
        return Ok(Some(proxy.clone()));
    }
    let route = crate::egress::proxy::from_env_for("https://cse.google.com/")
        .map_err(|e| super::tasks::engine_fetch_status(&e))?;
    let bootstrap = crate::egress::proxy::from_env_for("https://www.google.com/")
        .map_err(|e| super::tasks::engine_fetch_status(&e))?;
    if route.as_ref().map(|p| p.id()) != bootstrap.as_ref().map(|p| p.id()) {
        return Err("invalid-config: search bootstrap and query require the same route".into());
    }
    Ok(route)
}

fn token_key(proxy: Option<&crate::egress::proxy::Proxy>) -> String {
    format!(
        "{}|{}|{}",
        env!("CARGO_PKG_VERSION"),
        std::env::consts::OS,
        proxy.map(|p| p.id()).unwrap_or_else(|| "direct".into())
    )
}

fn bootstrap(script: &str) -> Option<Token> {
    let start = script.rfind("({")? + 1;
    let end = script.rfind("});")? + 1;
    let data: Value = serde_json::from_str(script.get(start..end)?).ok()?;
    let value = data["cse_token"].as_str()?.to_string();
    if value.is_empty() || value.len() > 8192 {
        return None;
    }
    let experiments = data["exp"]
        .as_array()
        .map(|values| {
            values
                .iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(",")
        })
        .unwrap_or_default();
    Some(Token {
        value,
        version: data["cselibVersion"].as_str().unwrap_or("").to_string(),
        experiments,
        written: now(),
    })
}

fn response(body: &str) -> Result<Vec<Hit>, String> {
    let start = body.find('{').ok_or("empty-parse")?;
    let end = body.rfind('}').ok_or("empty-parse")? + 1;
    let data: Value = serde_json::from_str(&body[start..end]).map_err(|_| "empty-parse")?;
    if let Some(error) = data.get("error") {
        return Err(match error["code"].as_u64() {
            Some(429) => "rate-limited:900".into(),
            Some(403) => "blocked:403".into(),
            Some(400) => "invalid-token".into(),
            Some(code) => format!("http:{code}"),
            None => "invalid-response".into(),
        });
    }
    let results = data["results"].as_array().ok_or("empty-parse")?;
    Ok(results
        .iter()
        .take(20)
        .enumerate()
        .filter_map(|(rank, r)| {
            let url = r["unescapedUrl"].as_str()?;
            let title = r["titleNoFormatting"].as_str().unwrap_or("");
            if title.is_empty() {
                return None;
            }
            Some(Hit {
                title: title.to_string(),
                url: url.to_string(),
                snippet: r["contentNoFormatting"].as_str().unwrap_or("").to_string(),
                rank,
                published: None,
            })
        })
        .collect())
}

fn search_url(query: &str, token: &Token, locale: Option<super::locale::Locale>) -> String {
    let mut url = url::Url::parse("https://cse.google.com/cse/element/v1").unwrap();
    {
        let mut q = url.query_pairs_mut();
        q.extend_pairs([
            ("rsz", "filtered_cse"),
            ("num", "20"),
            ("cx", CX),
            ("q", query),
            ("safe", "off"),
            ("cse_tok", &token.value),
            ("cselibv", &token.version),
            ("callback", "_"),
            ("rurl", ""),
            ("searchtype", ""),
        ]);
        if let Some(locale) = locale {
            q.append_pair("hl", locale.lang)
                .append_pair("gl", locale.region);
        } else if super::locale::latin_query(query) {
            q.append_pair("hl", "en").append_pair("gl", "us");
        }
        if !token.experiments.is_empty() {
            q.append_pair("exp", &token.experiments);
        }
    }
    url.into()
}

pub(super) async fn task(
    query: String,
    lane: String,
    proxy: Option<crate::egress::proxy::Proxy>,
    context: EngineContext<'_>,
    budget: Duration,
) -> (String, EngineResult) {
    let start = Instant::now();
    let work = async {
        context.pool.pace(SOURCE, &lane).await;
        // Resolve the configured route once, then pin both origins to it.
        let proxy = request_proxy(proxy.as_ref())?;
        let token_key = token_key(proxy.as_ref());
        let mut requests = 0;
        let cached = context.cse.get(&token_key);
        let token = match cached {
            Some(token) => token,
            None => {
                if !context.requests.take(SOURCE) {
                    return Err("budget-skipped".into());
                }
                requests += 1;
                let url = format!("https://www.google.com/cse/cse.js?cx={CX}");
                let out = context
                    .fetcher
                    .fetch_once_via(
                        &url,
                        &[],
                        proxy.as_ref(),
                        false,
                        Some("https://www.blackle.com/"),
                    )
                    .await
                    .map_err(|e| super::tasks::engine_fetch_status(&e))?;
                if let Some(rest) =
                    super::verticals::throttle(SOURCE, out.status, &out.headers, &out.body)
                {
                    return Err(format!("rate-limited:{}", rest.as_secs()));
                }
                if out.status != 200 {
                    return Err(format!("blocked:{}", out.status));
                }
                if matches!(out.verdict, crate::detect::walls::Verdict::Challenge(_)) {
                    return Err("blocked:challenge".into());
                }
                if out.body.len() > BODY_CAP {
                    return Err("blocked:oversize-serp".into());
                }
                let token =
                    bootstrap(&String::from_utf8_lossy(&out.body)).ok_or("invalid-token")?;
                context.cse.put(&token_key, Some(token.clone()));
                token
            }
        };
        if !context.requests.take(SOURCE) {
            return Err("budget-skipped".into());
        }
        requests += 1;
        let url = search_url(&query, &token, context.locale);
        let out = context
            .fetcher
            .fetch_once_via(
                &url,
                &[],
                proxy.as_ref(),
                false,
                Some("https://cse.google.com/"),
            )
            .await
            .map_err(|e| super::tasks::engine_fetch_status(&e))?;
        if let Some(rest) = super::verticals::throttle(SOURCE, out.status, &out.headers, &out.body)
        {
            return Err(format!("rate-limited:{}", rest.as_secs()));
        }
        if out.status != 200 {
            return Err(format!("blocked:{}", out.status));
        }
        if out.body.len() > BODY_CAP {
            return Err("blocked:oversize-serp".into());
        }
        let hits = match response(&String::from_utf8_lossy(&out.body)) {
            Ok(hits) => hits,
            Err(status) => {
                if status == "invalid-token" {
                    context.cse.put(&token_key, None);
                }
                return Err(status);
            }
        };
        let facts = super::query::QueryFacts::new(&query);
        let judged = super::quality::judge(context.facts.unwrap_or(&facts), hits);
        Ok(LaneAnswer {
            hits: judged.kept,
            items: judged.items,
            ms: start.elapsed().as_millis() as u64,
            egress: lane.clone(),
            engine_lane: true,
            instant: None,
            class: judged.class,
            requests,
            cache_age: None,
        })
    };
    let result = match tokio::time::timeout(budget, work).await {
        Ok(result) => result.map_err(|status| (status, lane, true)),
        Err(_) => Err(("timeout".into(), lane, true)),
    };
    (SOURCE.into(), result)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bootstrap_reads_json_without_executing_a_program() {
        let script =
            r#"evil(); init({"cse_token":"issued","cselibVersion":"abc","exp":["a","b"]});"#;
        let token = bootstrap(script).unwrap();
        let client = Client::default();
        let route = crate::egress::proxy::Proxy::parse("http://127.0.0.1:18081").unwrap();
        assert_eq!(
            client.request_cost(Some(&route)),
            2,
            "a cold bootstrap and query need two starts"
        );
        client
            .tokens
            .lock()
            .unwrap()
            .insert(token_key(Some(&route)), token.clone());
        assert_eq!(client.request_cost(Some(&route)), 1);
        let other = crate::egress::proxy::Proxy::parse("http://127.0.0.1:18082").unwrap();
        assert_eq!(
            client.request_cost(Some(&other)),
            2,
            "tokens cannot cross routes"
        );
        let mut nearly_expired = token.clone();
        nearly_expired.written = now().saturating_sub(TOKEN_TTL - 5);
        client
            .tokens
            .lock()
            .unwrap()
            .insert(token_key(Some(&route)), nearly_expired);
        assert_eq!(
            client.request_cost(Some(&route)),
            2,
            "dispatch must not outrun a token's lifetime"
        );
        assert_eq!(token.value, "issued");
        assert_eq!(token.experiments, "a,b");
        assert!(token.fresh(token.written));
        assert!(!token.fresh(token.written + TOKEN_TTL));
        assert!(!token.fresh(token.written - 1));
        assert!(bootstrap("init({cse_token: steal()});").is_none());
        assert!(bootstrap(r#"init({"cse_token":""});"#).is_none());
        let url = url::Url::parse(&search_url("tokio site:docs.rs -wrong", &token, None)).unwrap();
        assert_eq!(
            url.query_pairs().find(|(k, _)| k == "q").unwrap().1,
            "tokio site:docs.rs -wrong"
        );
        assert_eq!(url.query_pairs().find(|(k, _)| k == "hl").unwrap().1, "en");
        let han = url::Url::parse(&search_url("東京 地震 速報", &token, None)).unwrap();
        assert!(!han.query_pairs().any(|(key, _)| key == "hl" || key == "gl"));
    }
    #[test]
    fn structured_results_and_refusals_remain_distinct() {
        let hits = response(r#"_({"results":[{"unescapedUrl":"https://example.com/docs","titleNoFormatting":"Documentation","contentNoFormatting":"Real evidence"},{"titleNoFormatting":"missing URL"}]});"#).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].snippet, "Real evidence");
        assert_eq!(hits[0].rank, 0, "all sources use zero-based ranks");
        assert_eq!(
            response(r#"_({"error":{"code":429,"message":"do not echo"}});"#).unwrap_err(),
            "rate-limited:900"
        );
        assert_eq!(
            response(r#"_({"error":{"code":403}});"#).unwrap_err(),
            "blocked:403"
        );
        assert!(response("not JSON").is_err());
        assert!(response(r#"_({"unexpected":"schema"});"#).is_err());
        assert_eq!(
            response(r#"_({"error":{"code":500}});"#).unwrap_err(),
            "http:500"
        );
        assert!(response(r#"_({"results":[]});"#).unwrap().is_empty());
    }
}
