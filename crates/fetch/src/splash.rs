//! Market splashes: the site's own "stay on this market" signal.
//!
//! Several stores geolocate a client and answer its *first* request with a
//! chooser ("Best Buy International: Select your Country") instead of the page.
//! The chooser is the site's own document, and the site's own links carry the
//! thing a browser sends back to stay: a query pair its "United States" link
//! appends (`intl=nosplash`). Sending that pair is not a guess about the site —
//! it is the request the site's own splash page offers, one hop earlier.
//!
//! The table is closed and inspectable, like the data paths: a host, the query
//! pair its splash links carry, and nothing else. A host whose splash is
//! answered some other way is not here.

struct Splash {
    /// Host suffixes this signal serves.
    hosts: &'static [&'static str],
    /// The query pair the site's own splash link carries.
    pair: &'static str,
}

static SPLASHES: &[Splash] = &[
    // Best Buy's international splash is a 2xx page titled "Best Buy
    // International: Select your Country" served instead of the search page;
    // its own links (`/site/help-topics/international-orders/...&intl=nosplash`)
    // carry the pair that keeps the visit on the US market. With it the same
    // search URL answers the real page (`headphones - Best Buy`, 2026-11-27).
    Splash {
        hosts: &["bestbuy.com"],
        pair: "intl=nosplash",
    },
];

fn entry(host: &str) -> Option<&'static Splash> {
    SPLASHES.iter().find(|splash| {
        splash
            .hosts
            .iter()
            .any(|suffix| host == *suffix || host.ends_with(&format!(".{suffix}")))
    })
}

/// `url` with the host's market signal applied, if the host has one and the URL
/// does not carry it yet.
pub fn apply(host: &str, url: &str) -> Option<String> {
    let splash = entry(host)?;
    let (key, value) = splash.pair.split_once('=')?;
    let mut parsed = url::Url::parse(url).ok()?;
    if parsed
        .query_pairs()
        .any(|(name, _)| name.eq_ignore_ascii_case(key))
    {
        return None;
    }
    parsed.query_pairs_mut().append_pair(key, value);
    Some(parsed.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_store_with_a_splash_carries_its_own_market_pair() {
        assert_eq!(
            apply(
                "www.bestbuy.com",
                "https://www.bestbuy.com/site/searchpage.jsp?st=x"
            )
            .as_deref(),
            Some("https://www.bestbuy.com/site/searchpage.jsp?st=x&intl=nosplash")
        );
        // Already carrying it: the URL is left as the caller wrote it.
        assert_eq!(
            apply(
                "www.bestbuy.com",
                "https://www.bestbuy.com/site/searchpage.jsp?st=x&intl=nosplash"
            ),
            None
        );
        // Nothing to do for a host that has no signal.
        assert_eq!(apply("www.example.com", "https://www.example.com/"), None);
    }
}
