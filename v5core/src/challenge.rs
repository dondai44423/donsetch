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

/// SiteGround's bot check (`sg-captcha: challenge`).
///
/// The guarded page is a stub whose meta refresh leads to
/// `/.well-known/sgcaptcha/`. That page carries the whole challenge:
/// `sgchallenge` (`<bits>:<nonce material>`) and `sgsubmit_url`. Its program
/// searches for a counter whose SHA-1 over `challenge ‖ counter` starts with
/// `bits` zero bits, then navigates to `sgsubmit_url&sol=<base64 of the hashed
/// bytes>&s=<ms>:<hashes>`. The server answers with an `_I_` clearance cookie
/// bound to the client, and the stub's own URL then serves the page.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SgWork {
    pub challenge: String,
    pub submit: String,
}

/// The most work a SiteGround answer may cost: 2^24 hashes on average. Sites
/// ask 20-22 bits; a larger ask is not answered.
pub const SG_MAX_BITS: u32 = 24;

/// The challenge page a SiteGround stub's meta refresh leads to.
pub fn siteground_stub(body: &str) -> Option<String> {
    let at = body.find("/.well-known/sgcaptcha/")?;
    let rest = &body[at..];
    let end = rest.find(['"', '\'', '>', ' '])?;
    Some(rest[..end].replace("&amp;", "&"))
}

/// The work a SiteGround challenge page asks for.
pub fn siteground_work(body: &str) -> Option<SgWork> {
    let constant = |name: &str| {
        let at = body.find(&format!("const {name}=\""))? + name.len() + 8;
        let rest = &body[at..];
        Some(rest[..rest.find('"')?].to_string())
    };
    let challenge = constant("sgchallenge")?;
    let submit = constant("sgsubmit_url")?;
    let bits: u32 = challenge.split(':').next()?.parse().ok()?;
    (submit.starts_with("/.well-known/sgcaptcha/") && (1..=SG_MAX_BITS).contains(&bits))
        .then_some(SgWork { challenge, submit })
}

/// The counter's bytes as the page encodes them: big-endian, as few bytes as
/// hold it (at least one).
fn sg_counter(n: u32) -> ([u8; 4], usize) {
    let bytes = n.to_be_bytes();
    let used = (4 - n.leading_zeros() as usize / 8).max(1);
    (bytes, used)
}

/// Does this digest answer a `bits` challenge? Its first 32-bit word starts
/// with `bits` zero bits. The page's program treats an all-zero first word as
/// a failed hash, so it does here too.
fn sg_meets(digest: &[u8; 20], bits: u32) -> bool {
    let word = u32::from_be_bytes([digest[0], digest[1], digest[2], digest[3]]);
    word != 0 && word >> (32 - bits) == 0
}

/// Solve `work`: the `sol` value and how many hashes it took, or `None` when
/// the deadline passes first. Several threads search interleaved counters.
pub fn siteground_solve(work: &SgWork, deadline: std::time::Instant) -> Option<(String, u64)> {
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    let bits: u32 = work.challenge.split(':').next()?.parse().ok()?;
    if !(1..=SG_MAX_BITS).contains(&bits) || std::time::Instant::now() >= deadline {
        return None;
    }
    let prefix = work.challenge.as_bytes();
    let mut base = btls::sha::Sha1::new();
    base.update(prefix);
    let threads = std::thread::available_parallelism()
        .map_or(2, |n| n.get())
        .clamp(1, 4) as u32;
    let done = AtomicBool::new(false);
    let hashes = AtomicU64::new(0);
    let found: std::sync::Mutex<Option<u32>> = std::sync::Mutex::new(None);
    std::thread::scope(|scope| {
        for lane in 0..threads {
            let (base, done, hashes, found) = (&base, &done, &hashes, &found);
            scope.spawn(move || {
                // The page's workers start from a non-zero counter.
                let mut n = 1 + lane;
                let mut local = 0u64;
                while !done.load(Ordering::Relaxed) {
                    let (bytes, used) = sg_counter(n);
                    let mut hasher = base.clone();
                    hasher.update(&bytes[4 - used..]);
                    local += 1;
                    if sg_meets(&hasher.finish(), bits) {
                        done.store(true, Ordering::Relaxed);
                        if let Ok(mut slot) = found.lock() {
                            slot.get_or_insert(n);
                        }
                        break;
                    }
                    if local.is_multiple_of(4096) && std::time::Instant::now() >= deadline {
                        done.store(true, Ordering::Relaxed);
                    }
                    n = match n.checked_add(threads) {
                        Some(next) => next,
                        None => break,
                    };
                }
                hashes.fetch_add(local, Ordering::Relaxed);
            });
        }
    });
    let n = (*found.lock().ok()?)?;
    let (bytes, used) = sg_counter(n);
    let mut message = prefix.to_vec();
    message.extend_from_slice(&bytes[4 - used..]);
    Some((btls::base64::encode_block(&message), hashes.into_inner()))
}

/// Where the page's program navigates with its answer.
pub fn siteground_answer_path(work: &SgWork, sol: &str, ms: u128, hashes: u64) -> String {
    let joiner = if work.submit.contains('?') { '&' } else { '?' };
    let sol: String = sol
        .bytes()
        .map(|b| match b {
            b'A'..=b'Z'
            | b'a'..=b'z'
            | b'0'..=b'9'
            | b'-'
            | b'_'
            | b'.'
            | b'!'
            | b'~'
            | b'*'
            | b'\''
            | b'('
            | b')' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect();
    format!("{}{joiner}sol={sol}&s={ms}:{hashes}", work.submit)
}

/// The `/_pow/solve` proof of work (NCBI serves it, with HTTP 203, in front
/// of PubMed). The page carries `challengeId` and `bits`; its program counts a
/// decimal nonce up from 0 until SHA-256 over `<challengeId>:<nonce>` starts
/// with `bits` zero bits, then POSTs `{challenge_id, nonce, redirect_url}` as
/// JSON to `/_pow/solve` from the page. The server answers `{"success": true}`
/// with a clearance cookie, and the page reloads.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PowWork {
    pub challenge_id: String,
    pub bits: u32,
}

/// The most work a `/_pow/solve` answer may cost: 2^22 hashes on average.
/// Pages ask 12 bits; a larger ask than this is not answered.
pub const POW_MAX_BITS: u32 = 22;

/// Where the page's program sends its answer.
pub const POW_SOLVE_PATH: &str = "/_pow/solve";

/// The work a `/_pow/solve` page asks for.
pub fn pow_work(body: &str) -> Option<PowWork> {
    if !body.contains("fetch(\"/_pow/solve\"") {
        return None;
    }
    let after = |marker: &str| Some(&body[body.find(marker)? + marker.len()..]);
    let id = after("const challengeId = \"")?;
    let challenge_id = &id[..id.find('"')?];
    let bits: u32 = after("const bits = ")?
        .split(|c: char| !c.is_ascii_digit())
        .next()?
        .parse()
        .ok()?;
    let id_ok = !challenge_id.is_empty()
        && challenge_id.len() <= 128
        && challenge_id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    (id_ok && (1..=POW_MAX_BITS).contains(&bits)).then(|| PowWork {
        challenge_id: challenge_id.to_string(),
        bits,
    })
}

/// Does this digest start with `bits` zero bits?
fn pow_meets(digest: &[u8; 32], bits: u32) -> bool {
    u32::from_be_bytes([digest[0], digest[1], digest[2], digest[3]]).leading_zeros() >= bits
}

/// The page's nonce for `work`: the first one, counting from 0 as the page
/// does, or `None` when the deadline passes first.
pub fn pow_solve(work: &PowWork, deadline: std::time::Instant) -> Option<u64> {
    if !(1..=POW_MAX_BITS).contains(&work.bits) {
        return None;
    }
    let mut message = format!("{}:", work.challenge_id);
    let prefix = message.len();
    for nonce in 0u64.. {
        if nonce.is_multiple_of(4096) && std::time::Instant::now() >= deadline {
            return None;
        }
        message.truncate(prefix);
        message.push_str(&nonce.to_string());
        if pow_meets(&btls::sha::sha256(message.as_bytes()), work.bits) {
            return Some(nonce);
        }
    }
    None
}

/// The JSON the page's program posts with its answer.
pub fn pow_answer(work: &PowWork, nonce: u64, page_url: &str) -> String {
    serde_json::json!({
        "challenge_id": work.challenge_id,
        "nonce": nonce.to_string(),
        "redirect_url": page_url,
    })
    .to_string()
}

#[cfg(test)]
mod pow_tests {
    use super::*;

    // The page's own constants and the call that names the contract, as
    // pubmed.ncbi.nlm.nih.gov served them (2026-11-08).
    const PAGE: &str = r#"<script>
const challengeId = "MpNBUn2OifdS_VNrp-IRTA";
const challengeDomain = "pubmed.ncbi.nlm.nih.gov";
const bits = 12;
const maxMs = 40000;
async function submitSolve(nonce) {
  const response = await fetch("/_pow/solve", {method: "POST"});
}
</script>"#;

    #[test]
    fn the_page_states_its_work() {
        assert_eq!(
            pow_work(PAGE),
            Some(PowWork {
                challenge_id: "MpNBUn2OifdS_VNrp-IRTA".into(),
                bits: 12
            })
        );
        // Too much work, an id that is not a token, or no solve call.
        assert!(pow_work(&PAGE.replace("bits = 12", "bits = 30")).is_none());
        assert!(pow_work(&PAGE.replace("bits = 12", "bits = 0")).is_none());
        assert!(pow_work(&PAGE.replace("MpNBUn2OifdS_VNrp-IRTA", "a b")).is_none());
        assert!(pow_work(&PAGE.replace("/_pow/solve", "/other")).is_none());
        assert!(pow_work("<p>an article about const bits = 12</p>").is_none());
    }

    #[test]
    fn the_nonce_is_the_pages_first_answer() {
        let work = pow_work(PAGE).unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let nonce = pow_solve(&work, deadline).unwrap();
        let digest = btls::sha::sha256(format!("{}:{nonce}", work.challenge_id).as_bytes());
        assert!(pow_meets(&digest, 12));
        // No smaller nonce answers: the page counts up from 0.
        assert!((0..nonce).all(|n| {
            let digest = btls::sha::sha256(format!("{}:{n}", work.challenge_id).as_bytes());
            !pow_meets(&digest, 12)
        }));
        let answer: serde_json::Value = serde_json::from_str(&pow_answer(
            &work,
            nonce,
            "https://pubmed.ncbi.nlm.nih.gov/1/",
        ))
        .unwrap();
        assert_eq!(answer["challenge_id"], "MpNBUn2OifdS_VNrp-IRTA");
        assert_eq!(answer["nonce"], nonce.to_string());
        assert_eq!(answer["redirect_url"], "https://pubmed.ncbi.nlm.nih.gov/1/");
        // A deadline already passed answers nothing.
        assert!(pow_solve(&work, std::time::Instant::now()).is_none());
    }

    #[test]
    fn leading_zero_bits_count_across_bytes() {
        let mut digest = [0xffu8; 32];
        digest[0] = 0;
        digest[1] = 0x0f;
        assert!(pow_meets(&digest, 12));
        assert!(!pow_meets(&digest, 13));
    }
}

#[cfg(test)]
mod siteground_tests {
    use super::*;

    const STUB: &str = r#"<html><head><link rel="icon" href="data:;"><meta http-equiv="refresh" content="0;/.well-known/sgcaptcha/?r=%2Farticles%2Fx&y=ipc:1.2.3.4:1794082224.253"></meta></head></html>"#;

    #[test]
    fn the_stub_names_the_challenge_page() {
        assert_eq!(
            siteground_stub(STUB).as_deref(),
            Some("/.well-known/sgcaptcha/?r=%2Farticles%2Fx&y=ipc:1.2.3.4:1794082224.253")
        );
        assert_eq!(
            siteground_stub("<html><p>about /.well-known/</p></html>"),
            None
        );
    }

    #[test]
    fn the_challenge_page_carries_its_work() {
        let page = r#"<script>const sgchallenge="12:1794082256:7189c18e:9b5e:";
const sgsubmit_url="/.well-known/sgcaptcha/?r=%2Fa";const sgfallback_url="/x";</script>"#;
        let work = siteground_work(page).unwrap();
        assert_eq!(work.challenge, "12:1794082256:7189c18e:9b5e:");
        assert_eq!(work.submit, "/.well-known/sgcaptcha/?r=%2Fa");
        // Too much work, or a submit address off the challenge path, is refused.
        assert!(siteground_work(&page.replace("\"12:", "\"30:")).is_none());
        assert!(siteground_work(
            &page.replace("/.well-known/sgcaptcha/?r", "https://x.example/?r")
        )
        .is_none());
    }

    #[test]
    fn the_counter_is_minimal_big_endian() {
        let enc = |n| {
            let (b, used) = sg_counter(n);
            b[4 - used..].to_vec()
        };
        assert_eq!(enc(0), vec![0]);
        assert_eq!(enc(255), vec![255]);
        assert_eq!(enc(256), vec![1, 0]);
        assert_eq!(enc(0x01_00_00_00), vec![1, 0, 0, 0]);
    }

    #[test]
    fn a_solution_meets_the_ask_and_encodes_the_hashed_bytes() {
        let work = SgWork {
            challenge: "14:1794082256:7189c18e:9b5e9f1d:".into(),
            submit: "/.well-known/sgcaptcha/?r=%2Fa".into(),
        };
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        let (sol, hashes) = siteground_solve(&work, deadline).unwrap();
        assert!(hashes > 0);
        // The answer decodes to challenge ‖ counter and its SHA-1 meets 14 bits.
        let decoded = {
            let mut out = Vec::new();
            let table = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
            let (mut acc, mut nbits) = (0u32, 0);
            for c in sol.trim_end_matches('=').chars() {
                acc = (acc << 6) | table.find(c).unwrap() as u32;
                nbits += 6;
                if nbits >= 8 {
                    nbits -= 8;
                    out.push((acc >> nbits) as u8);
                    acc &= (1 << nbits) - 1;
                }
            }
            out
        };
        assert!(decoded.starts_with(work.challenge.as_bytes()));
        assert!(sg_meets(&btls::sha::sha1(&decoded), 14));
        let path = siteground_answer_path(&work, &sol, 120, hashes);
        assert!(
            path.starts_with("/.well-known/sgcaptcha/?r=%2Fa&sol="),
            "{path}"
        );
        let value = &path[path.find("sol=").unwrap() + 4..path.find("&s=").unwrap()];
        assert!(!value.contains(['+', '/', '=']), "{path}");
    }

    #[test]
    fn a_passed_deadline_gives_up() {
        let work = SgWork {
            challenge: "24:1:2:3:".into(),
            submit: "/.well-known/sgcaptcha/?r=%2Fa".into(),
        };
        assert!(siteground_solve(&work, std::time::Instant::now()).is_none());
    }
}
