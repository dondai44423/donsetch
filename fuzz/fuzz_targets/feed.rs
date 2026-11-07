//! Fuzz feed detection + extraction: arbitrary bytes judged as
//! RSS/Atom/JSON Feed. Structured rendering must never panic on
//! malformed feeds (truncated XML, nested channel weirdness,
//! hostile date strings).

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let ct = if data.first() == Some(&b'j') {
        "application/json"
    } else {
        "application/rss+xml"
    };
    if donsetch::extract::feed::is_feed(ct, data) {
        let opts = donsetch::extract::ExtractOptions::default();
        let _ = donsetch::extract::feed::extract(data, ct, "https://fuzz.example.com/feed", &opts);
    }
});
