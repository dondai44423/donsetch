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
    let lower = body.to_ascii_lowercase();
    let at = lower.find("http-equiv=\"refresh\"")?;
    let rest = &body[at..];
    let end = rest
        .find('>')
        .map(|offset| at + offset)
        .unwrap_or(body.len());
    let tag = &body[at..end];
    let delay = tag
        .split("content=")
        .nth(1)?
        .trim_start_matches(['"', '\''])
        .split(|character: char| !character.is_ascii_digit())
        .next()?
        .parse::<u64>()
        .unwrap_or(5);
    let url_at = tag.find("URL=")?;
    let mut target = tag[url_at + 4..]
        .trim_start_matches(['"', '\''])
        .to_string();
    if let Some(quote) = target.find(['"', '\'']) {
        target.truncate(quote);
    }
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
    fn the_wait_is_bounded() {
        let body = r#"<meta http-equiv="refresh" content="99; URL='/?bm-verify=x'">"#;
        assert_eq!(akamai_bm_verify(body).unwrap().delay_secs, 6);
    }
}
