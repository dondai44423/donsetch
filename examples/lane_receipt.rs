//! Scratch live receipt: a URL fetched through the REAL proxy pool.
//! Prints the fetch error, the per-lane state after, and the persisted
//! dead map, so lane-health behavior is observable end to end.
//!
//! Run: DONSETCH_CACHE_DIR=<dir with proxies.txt> \
//!      cargo run --profile fast --example lane_receipt [url]

use donsetch::fetch::client::Fetcher;
use donsetch::profile::BrowserProfile;
use donsetch::search::egress::EgressPool;

#[tokio::main]
async fn main() {
    let pool = std::sync::Arc::new(EgressPool::from_env());
    let lanes: Vec<String> = pool.proxies().iter().map(|p| p.id()).collect();
    println!("lanes: {}", lanes.len());
    for l in &lanes {
        println!("  {l}");
    }

    let fetcher = Fetcher::new(BrowserProfile::host_default())
        .expect("fetcher")
        .with_egress(std::sync::Arc::clone(&pool));

    let url = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "https://self-signed.badssl.com/".into());
    match fetcher.fetch(&url).await {
        Ok(o) => println!("RESULT ok status={} verdict={:?}", o.status, o.verdict),
        Err(e) => println!("RESULT err: {e}"),
    }

    println!("--- lane states after the fetch:");
    for row in pool.lane_summary() {
        if !row.is_direct {
            println!("  {} => {}", row.id, row.state);
        }
    }

    let Some(dir) = std::env::var_os("DONSETCH_CACHE_DIR") else {
        println!("--- DONSETCH_CACHE_DIR not set; skipping health dump");
        return;
    };
    let path = std::path::Path::new(&dir).join("egress-health.json");
    match std::fs::read_to_string(&path) {
        Ok(raw) => {
            let v: serde_json::Value = serde_json::from_str(&raw).unwrap_or_default();
            println!(
                "--- egress-health dead: {}",
                v.get("dead").cloned().unwrap_or_default()
            );
        }
        Err(_) => println!("--- no egress-health.json at {}", path.display()),
    }
}
