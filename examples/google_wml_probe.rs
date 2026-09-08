//! Explicit live probe: cargo run --profile ci --example google_wml_probe -- "rust programming language"
//! One direct HTTP request per query, no browser, proxy, API key or persisted search state.
//! Optional GOOGLE_PROBE_BASELINE=1 also tests the ordinary desktop endpoint.
//! GOOGLE_PROBE_USER_AGENT overrides only this experiment's WML identity.
use donsetch::{fetch::client::Fetcher, profile::BrowserProfile, search::engines};
use std::time::{Duration, Instant};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let queries: Vec<_> = std::env::args().skip(1).collect();
    if queries.is_empty() || queries.len() > 5 {
        return Err("provide 1–5 public, non-sensitive queries".into());
    }
    let fetcher = Fetcher::new(BrowserProfile::host_default())?;
    let user_agent = match std::env::var("GOOGLE_PROBE_USER_AGENT") {
        Ok(ua) => ua,
        Err(_) => engines::google_wml::configured_user_agent()
            .ok_or("invalid DONSETCH_GOOGLE_PROFILE")?
            .to_owned(),
    };
    let mut failed = false;
    for query in queries {
        let modes = if std::env::var_os("GOOGLE_PROBE_BASELINE").is_some() {
            vec!["google", "google_ghost"]
        } else {
            vec!["google"]
        };
        for mode in modes {
            let started = Instant::now();
            let url = engines::serp_url(mode, &query).unwrap();
            let response = tokio::time::timeout(Duration::from_secs(8), async {
                if mode == "google" {
                    fetcher
                        .fetch_once_via_user_agent(&url, None, &user_agent)
                        .await
                } else {
                    // Desktop endpoint over HTTP only; no ghost browser is started.
                    fetcher.fetch_once_via(&url, &[], None, false, None).await
                }
            })
            .await;
            match response {
                Ok(Ok(out)) => {
                    let content_type = out
                        .headers
                        .iter()
                        .find(|(n, _)| n.eq_ignore_ascii_case("content-type"))
                        .map(|(_, v)| v.as_str())
                        .unwrap_or("");
                    let html = donsetch::extract::charset::decode(&out.body, content_type);
                    let error =
                        engines::google_wml::response_error(out.status, &out.headers, &html);
                    let hits = engines::parse(mode, &html);
                    let ok = out.status == 200
                        && out.verdict == donsetch::detect::walls::Verdict::ContentOk
                        && error.is_none()
                        && hits.len() >= 3;
                    failed |= mode == "google" && !ok;
                    println!(
                        "{}",
                        serde_json::json!({
                            "query":query, "mode":mode, "browser":false, "status":out.status,
                            "user_agent":if mode == "google" { &user_agent } else { &fetcher.profile().user_agent },
                            "alpn":out.alpn, "bytes":out.body.len(), "ms":started.elapsed().as_millis(),
                            "verdict":format!("{:?}",out.verdict), "error":error,
                            "hits":hits.len(), "ok":ok,
                            "results":hits.iter().take(5).map(|h| serde_json::json!({"title":h.title,"url":h.url,"snippet":h.snippet})).collect::<Vec<_>>()
                        })
                    );
                    if out.status != 200
                        || out.verdict != donsetch::detect::walls::Verdict::ContentOk
                        || error.is_some()
                    {
                        // No retry/UA rotation to hammer a blocked endpoint.
                        return Err("Google blocked or redirected the probe; stopping".into());
                    }
                }
                other => {
                    eprintln!(
                        "query={query:?} mode={mode}: {}",
                        match other {
                            Ok(Err(e)) => e.to_string(),
                            Err(_) => "timeout".into(),
                            _ => unreachable!(),
                        }
                    );
                    return Err("native HTTP probe failed".into());
                }
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    }
    if failed {
        return Err("one or more WML queries produced no usable results".into());
    }
    Ok(())
}
