//! Native challenge contracts: the site's own proof-of-work, implemented as code.
//!
//! Not a renderer and not an interpreter. Each entry is a bounded, inspectable
//! rule: recognise the challenge, do exactly what the site asks (usually one
//! delayed request carrying its token), and stop. A challenge that needs a page's
//! program to run is *not* here — it stays a declared class.

/// Akamai Bot Manager's no-JavaScript path.
///
/// A client that Akamai does not want to challenge outright receives the page it
/// asked for plus `ak_bmsc`, and a meta refresh to `?bm-verify=<token>`. Following
/// that URL after the declared delay, from the same client and with the cookies
/// intact, is what a browser without JavaScript does — and what the sensor sees as
/// legitimate. The token is bound to the client that received it, which is why this
/// only works as one identity end to end.
#[derive(Clone, Debug)]
pub struct VerifyRedirect {
    /// Path and query to request next.
    pub target: String,
    /// Seconds the page itself asks the client to wait.
    pub delay_secs: u64,
}

/// Recognise a `bm-verify` meta refresh and return where to go and how long to wait.
pub fn akamai_bm_verify(body: &str) -> Option<VerifyRedirect> {
    if !body.contains("bm-verify") {
        return None;
    }
    // The whole `<meta>` tag: its attributes come in any order and any quoting.
    let lower = body.to_ascii_lowercase();
    let refresh = [
        "http-equiv=\"refresh\"",
        "http-equiv='refresh'",
        "http-equiv=refresh",
    ]
    .iter()
    .filter_map(|needle| lower.find(needle))
    .min()?;
    let start = lower[..refresh].rfind('<').unwrap_or(0);
    let end = lower[refresh..]
        .find('>')
        .map_or(body.len(), |offset| refresh + offset);
    let tag = &body[start..end];
    let lower_tag = tag.to_ascii_lowercase();
    let delay = tag[lower_tag.find("content=")? + "content=".len()..]
        .trim_start_matches(['"', '\''])
        .split(|character: char| !character.is_ascii_digit())
        .next()?
        .parse::<u64>()
        .unwrap_or(5);
    // `URL=` is case-insensitive (HTML's refresh syntax), and the target is
    // attribute text: entities such as `&amp;` are the page's, not the URL's.
    let url_at = lower_tag.find("url=")?;
    let mut target = tag[url_at + 4..]
        .trim_start_matches(['"', '\''])
        .to_string();
    if let Some(quote) = target.find(['"', '\'']) {
        target.truncate(quote);
    }
    let target = target.trim().replace("&amp;", "&");
    if !target.contains("bm-verify") {
        return None;
    }
    Some(VerifyRedirect {
        target,
        delay_secs: delay.min(6),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognises_the_akamai_handoff() {
        let body = r#"<html><head><meta http-equiv="refresh" content="5; URL='/?bm-verify=AAQAAAAO-x&x=1'"></head></html>"#;
        let found = akamai_bm_verify(body).expect("challenge");
        assert_eq!(found.target, "/?bm-verify=AAQAAAAO-x&x=1");
        assert_eq!(found.delay_secs, 5);
    }

    #[test]
    fn ignores_pages_that_only_mention_the_word() {
        let body = "<html><body>nothing about it</body></html>";
        assert!(akamai_bm_verify(body).is_none());
        let body = r#"<meta http-equiv="refresh" content="3; URL='/other'">"#;
        assert!(akamai_bm_verify(body).is_none());
    }

    #[test]
    fn the_refresh_is_read_as_html_writes_it() {
        let body = r#"<META HTTP-EQUIV="Refresh" CONTENT="2;url=/p?bm-verify=T&amp;a=1">"#;
        let found = akamai_bm_verify(body).expect("challenge");
        assert_eq!(found.target, "/p?bm-verify=T&a=1");
        assert_eq!(found.delay_secs, 2);
        // Attributes in the other order.
        let body = r#"<meta content="3; URL='/?bm-verify=Z'" http-equiv="refresh">"#;
        let found = akamai_bm_verify(body).expect("challenge");
        assert_eq!(
            (found.target.as_str(), found.delay_secs),
            ("/?bm-verify=Z", 3)
        );
    }

    #[test]
    fn the_wait_is_bounded() {
        let body = r#"<meta http-equiv="refresh" content="99; URL='/?bm-verify=x'">"#;
        assert_eq!(akamai_bm_verify(body).unwrap().delay_secs, 6);
    }
}
