//! Memory-discipline soak (v3 foundation #4): the hot paths
//! (extraction, handle table, page history) churned hard, RSS
//! growth asserted bounded. A creeping daemon is a dying daemon :
//! this gate makes the ceiling a build failure, not a surprise.

#[cfg(target_os = "linux")]
fn rss_kb() -> u64 {
    let status = std::fs::read_to_string("/proc/self/status").unwrap();
    for line in status.lines() {
        if let Some(v) = line.strip_prefix("VmRSS:") {
            return v.trim().trim_end_matches(" kB").parse().unwrap_or(0);
        }
    }
    0
}

#[test]
#[cfg(target_os = "linux")]
fn soak_rss_stays_bounded() {
    // First: the handle table and page history load from the cache
    // root after the baseline is taken, so the developer's real
    // page-history.json would count toward the growth bound.
    crate::sandbox();
    let corpus = std::fs::read(format!(
        "{}/tests/fixtures/corpus/wiki-rust.html",
        env!("CARGO_MANIFEST_DIR")
    ))
    .expect("corpus fixture");
    let baseline = rss_kb();

    // 200 extraction cycles (full pipeline + focus + toc + probe).
    for i in 0..200 {
        let opts = donsetch::extract::ExtractOptions {
            max_chars: Some(20_000),
            ..match i % 4 {
                1 => donsetch::extract::ExtractOptions {
                    focus: Some("ownership borrow checker".into()),
                    ..Default::default()
                },
                2 => donsetch::extract::ExtractOptions {
                    toc: true,
                    ..Default::default()
                },
                3 => donsetch::extract::ExtractOptions {
                    must_contain: Some("borrow".into()),
                    ..Default::default()
                },
                _ => Default::default(),
            }
        };
        let ex = donsetch::extract::extract(
            &corpus,
            "text/html",
            "https://fixture.invalid/wiki/Soak",
            &opts,
        )
        .unwrap();
        assert!(!ex.markdown.is_empty());
    }

    // Handle table churn: 10k links through the 2048-slot LRU.
    let mut ht = donsetch::handles::HandleTable::load(); // no flush : never persists
    for i in 0..10_000 {
        ht.intern_link(&format!("https://example.com/page/{i}"));
    }
    for i in 0..500 {
        let _ = ht.resolve(&format!("L{i}"));
    }

    // Page history churn: 800 records through the 512-URL budget.
    let mut hist = donsetch::pages::history::PageHistory::load(); // no flush
    let long_text = "x".repeat(80_000);
    for i in 0..800 {
        hist.record(
            &format!("https://example.com/h/{i}"),
            &format!("fp{i}"),
            long_text.len(),
            Some("t"),
            if i % 2 == 0 { &long_text } else { "" },
        );
    }

    let after = rss_kb();
    let growth_mb = (after.saturating_sub(baseline)) as f64 / 1024.0;
    eprintln!("soak: baseline {baseline} kB → {after} kB (growth {growth_mb:.1} MB)");
    assert!(
        growth_mb < 100.0,
        "RSS grew {growth_mb:.1} MB over the soak : a store is leaking"
    );
}
