//! Capture a page once through search's native read path for evidence diagnosis.
//! Run with a private cache after checking the source's active refusal wait:
//! cargo run --profile fast --example search_evidence_probe -- URL QUERY OUT.json
//! No retries, alternate identities, browsers or providers; never qualification.
//! Re-select a captured page without network: --markdown CAPTURE.json QUERY OUT.json

use donsetch::{detect::walls::Verdict, extract, fetch::client::Fetcher, profile::BrowserProfile};
use std::{
    path::PathBuf,
    time::{Duration, Instant},
};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args.first().is_some_and(|a| a == "--markdown") && args.len() == 4 {
        let capture: serde_json::Value = serde_json::from_slice(&std::fs::read(&args[1])?)?;
        let markdown = capture["markdown"]
            .as_str()
            .ok_or("capture has no accepted page markdown")?;
        let receipt = serde_json::json!({
            "diagnostic_only": true, "network": false, "capture": args[1], "query": args[2],
            "passage": donsetch::search::evidence::passage(&args[2], markdown, 320),
        });
        std::fs::write(&args[3], serde_json::to_vec_pretty(&receipt)?)?;
        println!("{}", receipt["passage"]);
        return Ok(());
    }
    if args.len() != 3 {
        return Err("expected URL QUERY OUT.json".into());
    }
    let route = donsetch::egress::request_route::RequestRoute::configured();
    let proxy = route.proxy_for(&args[0])?;
    let fetcher = Fetcher::new(BrowserProfile::host_default())?;
    let started = Instant::now();
    let response = tokio::time::timeout(
        Duration::from_secs(8),
        fetcher.fetch_once_via(&args[0], &[], proxy.as_ref(), true, None),
    )
    .await??;
    let content_type = response
        .headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("content-type"))
        .map(|(_, v)| v.as_str())
        .unwrap_or("");
    let mut receipt = serde_json::json!({
        "diagnostic_only": true, "query": args[1], "url": response.url,
        "status": response.status, "verdict": format!("{:?}", response.verdict),
        "bytes": response.body.len(), "http_ms": started.elapsed().as_millis(),
        "retry_after": response.headers.iter().find(|(k,_)| k.eq_ignore_ascii_case("retry-after")).map(|(_,v)|v),
    });
    let json = content_type.contains("json");
    let accepted = response.status == 200
        && response.verdict == Verdict::ContentOk
        && (content_type.is_empty()
            || content_type.contains("html")
            || content_type.starts_with("text/")
            || json);
    if accepted && json && response.body.len() <= 3 << 20 {
        receipt["json"] = serde_json::from_slice(&response.body)?;
    } else if accepted && !json {
        // Retain the accepted source structure for parser regressions. Wall
        // responses never enter this branch and are metadata-only receipts.
        if content_type.contains("html") && response.body.len() <= 3 << 20 {
            receipt["html"] = serde_json::json!(String::from_utf8_lossy(&response.body));
        }
        let options = extract::ExtractOptions {
            max_chars: Some(64_000),
            ..Default::default()
        };
        let page = extract::extract(&response.body, content_type, &response.url, &options)
            .map_err(|error| format!("extraction: {error:?}"))?;
        receipt["passage"] = serde_json::json!(donsetch::search::evidence::passage(
            &args[1],
            &page.markdown,
            320
        ));
        receipt["markdown"] = serde_json::json!(page.markdown);
        receipt["extracted_bytes"] = serde_json::json!(page.total_chars);
        receipt["thin"] = serde_json::json!(page.thin);
    }
    let destination = PathBuf::from(&args[2]);
    std::fs::write(&destination, serde_json::to_vec_pretty(&receipt)?)?;
    println!(
        "status={} accepted={} http_ms={} receipt={}",
        response.status,
        accepted,
        receipt["http_ms"],
        destination.display()
    );
    if !accepted {
        return Err("source did not supply accepted content; stop and honor its wait".into());
    }
    Ok(())
}
