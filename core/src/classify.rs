//! Wall classification: the first-class primitive of the design.
//!
//! Every response is sorted into a class *before* anything is done about it, so
//! work only ever starts on the classes that are ours. Two rules keep this
//! honest:
//!
//!   * content is content only if the contract passes — never because the status
//!     was 200 or the body was large;
//!   * a wall's class comes from the vendor and what it asks for, not from
//!     keywords that appear in ordinary pages (a bare "captcha" string is in half
//!     the web's JavaScript and decides nothing).

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Class {
    /// Real content: the contract passed.
    Content,
    /// A wall our stored state should satisfy: a clearance or session is missing.
    WallState,
    /// A challenge we can answer natively: the served page carries its payload.
    WallContract,
    /// Refused for identity/egress reasons, nothing asked of us.
    WallIdentity,
    /// The site asks a human: slider, press-and-hold, captcha, Turnstile.
    WallInteraction,
    /// The site requires the page's own program to have run (a managed JS
    /// challenge, or a client-rendered application). The engine runs it for the
    /// vendor shapes it supports (AWS WAF); anything else is reported, not answered.
    WallProgram,
    /// 2xx but a client-rendered shell: no content without a renderer.
    Shell,
    /// 2xx, contract failed, and not a class we can name.
    Incomplete,
    /// No response at all (DNS, TLS, timeout, reset).
    Network,
    /// An HTTP error status with no wall markers.
    HttpError,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Wall {
    pub vendor: String,
    pub marker: String,
    pub asks: &'static str,
}

#[derive(Clone, Debug, Serialize)]
pub struct Verdict {
    pub class: Class,
    pub wall: Option<Wall>,
    pub evidence: Vec<String>,
}

struct Signature {
    vendor: &'static str,
    /// Body markers that identify the vendor, strongest first.
    markers: &'static [&'static str],
}

const SIGNATURES: &[Signature] = &[
    // A human widget is the most specific thing a page can ask for: check it
    // before the vendor that hosts it.
    Signature {
        vendor: "turnstile",
        markers: &["cf-turnstile", "challenges.cloudflare.com/turnstile"],
    },
    Signature {
        vendor: "recaptcha",
        markers: &["recaptcha/api.js", "gstatic.com/recaptcha", "g-recaptcha"],
    },
    Signature {
        vendor: "hcaptcha",
        markers: &["hcaptcha.com/1/api.js", "h-captcha"],
    },
    Signature {
        vendor: "cloudflare",
        markers: &[
            "cf_chl_opt",
            "__cf_chl",
            "/cdn-cgi/challenge-platform/",
            "cf-browser-verification",
            "cf-challenge",
        ],
    },
    Signature {
        vendor: "datadome",
        markers: &["captcha-delivery.com", "datadome"],
    },
    Signature {
        vendor: "perimeterx",
        markers: &["px-captcha", "_px3", "_pxvid", "perimeterx"],
    },
    Signature {
        vendor: "akamai",
        markers: &["_abck", "sec-if-cpt", "bm-verify", "akamai bot manager"],
    },
    Signature {
        vendor: "imperva",
        markers: &["incapsula", "_incap_ses", "visid_incap"],
    },
    Signature {
        vendor: "kasada",
        markers: &["kpsdk", "x-kpsdk-ct", "ips.js"],
    },
    Signature {
        vendor: "reddit",
        markers: &["jsc_token"],
    },
    Signature {
        vendor: "ebay-block",
        markers: &["pardon our interruption"],
    },
];

/// What a vendor page is asking for. This is the whole classification: vendor
/// identity plus the shape of the ask, never a keyword float.
fn asks(vendor: &str, lower: &str) -> (&'static str, Class) {
    match vendor {
        // Cloudflare's managed challenge ships its own payload; a native contract
        // can answer it. A hard block (1020 / "Attention Required") is not a
        // challenge at all.
        "cloudflare" => {
            // The legacy jschl form is arithmetic a native contract can answer.
            // The modern managed challenge is the page's own program: it cannot
            // be answered without running it, and answering it is out of scope.
            if lower.contains("jschl_vc") || lower.contains("jschl-answer") {
                ("legacy js challenge", Class::WallContract)
            } else if lower.contains("attention required")
                || lower.contains("you have been blocked")
            {
                ("block page", Class::WallIdentity)
            } else {
                ("managed challenge (page program)", Class::WallProgram)
            }
        }
        // DataDome serves an interstitial then a slider; the slider is human.
        "datadome" => {
            if lower.contains("captcha-delivery.com/captcha") || lower.contains("px-captcha") {
                ("slider captcha", Class::WallInteraction)
            } else {
                ("interstitial", Class::WallInteraction)
            }
        }
        "perimeterx" => {
            if lower.contains("px-captcha")
                || lower.contains("press &amp; hold")
                || lower.contains("press & hold")
            {
                ("press and hold", Class::WallInteraction)
            } else {
                ("px block", Class::WallIdentity)
            }
        }
        "turnstile" => ("turnstile widget", Class::WallInteraction),
        "recaptcha" => ("recaptcha widget", Class::WallInteraction),
        "hcaptcha" => ("hcaptcha widget", Class::WallInteraction),
        // Akamai/Kasada/Imperva send a sensor payload the client is expected to
        // compute; that is a contract, not a human step.
        "akamai" => ("sensor payload", Class::WallContract),
        "kasada" => ("sdk payload", Class::WallContract),
        // Reddit's own gate ships a form whose one field its inline program
        // computes and submits: the page's own program, so the engine runs it.
        // A page of the same origin with no form names no program.
        "reddit" => {
            if lower.contains("js_challenge") {
                ("js challenge form", Class::WallProgram)
            } else {
                ("block page", Class::WallIdentity)
            }
        }
        "imperva" => ("incident page", Class::WallIdentity),
        _ => ("block page", Class::WallIdentity),
    }
}

/// Phrases that mean "a human is being asked", used only when no vendor signature
/// matched, and only in their unambiguous forms.
pub(crate) const ASKS_HUMAN: &[&str] = &[
    "verify you are human",
    "verify you are a human",
    "confirm you are human",
    "are you a robot",
    "prove your humanity",
    "press &amp; hold",
    "press & hold",
    "press and hold the button",
    "activate and hold the button",
    "confirm that you're human",
    "confirm that you are human",
    "confirm you're human",
    "verify you're human",
    "verify you're a human",
    "verify that you're human",
    "slide right to verify",
    "drag the slider",
    "human verification",
    "checking if the site connection is secure",
    "enable javascript and cookies to continue",
    "checking your browser before accessing",
];

/// A refusal with no page of its own.
const REFUSALS: &[&str] = &[
    "access denied",
    "you don't have permission to access",
    "request blocked",
    "forbidden",
    "unusual traffic from your computer network",
    "rate limit",
    "too many requests",
];

pub struct Evidence<'a> {
    pub status: u16,
    pub body: &'a str,
    pub content_ok: bool,
    /// Response headers: vendors name themselves here before any page loads.
    pub headers: &'a [(String, String)],
}

pub fn classify(evidence: Evidence<'_>) -> Verdict {
    let lower = evidence.body.to_ascii_lowercase();
    let mut found: Vec<String> = Vec::new();
    let mut wall: Option<Wall> = None;

    // SiteGround names its challenge in a header on every page of it, and its
    // stub is a bare meta refresh that a generic contract cannot tell from a
    // short page: the header decides before the contract does.
    if evidence.headers.iter().any(|(key, value)| {
        key.eq_ignore_ascii_case("sg-captcha") && value.eq_ignore_ascii_case("challenge")
    }) {
        found.push("siteground:sg-captcha".into());
        return Verdict {
            class: Class::WallContract,
            wall: Some(Wall {
                vendor: "siteground".into(),
                marker: "sg-captcha".into(),
                asks: "proof of work",
            }),
            evidence: found,
        };
    }

    // The `/_pow/solve` proof of work (NCBI) is a short page whose program
    // states its work: the page names its own contract.
    if crate::challenge::pow_work(evidence.body).is_some() {
        found.push("pow:/_pow/solve".into());
        return Verdict {
            class: Class::WallContract,
            wall: Some(Wall {
                vendor: "pow-solve".into(),
                marker: "/_pow/solve".into(),
                asks: "proof of work",
            }),
            evidence: found,
        };
    }

    if evidence.content_ok {
        return Verdict {
            class: Class::Content,
            wall: None,
            evidence: found,
        };
    }

    // A vendor that names itself in the headers is telling us what it is, before
    // any page body is worth reading. Headers come first because challenge stubs
    // are often empty documents.
    let header = |name: &str| -> Option<&str> {
        evidence
            .headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    };
    let from_header = if header("x-amzn-waf-action").is_some() {
        Some((
            "aws-waf",
            "WAF challenge",
            Class::WallProgram,
            "x-amzn-waf-action",
        ))
    } else if header("x-datadome").is_some() || header("x-datadome-cid").is_some() {
        Some((
            "datadome",
            "automated-traffic check",
            Class::WallInteraction,
            "x-datadome",
        ))
    } else if header("cf-mitigated").is_some() {
        Some((
            "cloudflare",
            "managed challenge",
            Class::WallProgram,
            "cf-mitigated",
        ))
    } else if header("x-px").is_some() || header("x-px-block").is_some() {
        Some(("perimeterx", "human check", Class::WallInteraction, "x-px"))
    } else {
        None
    };
    if let Some((vendor, ask, class, marker)) = from_header {
        found.push(format!("{vendor}:{marker}"));
        return Verdict {
            class,
            wall: Some(Wall {
                vendor: vendor.to_string(),
                marker: marker.to_string(),
                asks: ask,
            }),
            evidence: found,
        };
    }

    // AWS WAF also serves the same challenge without its header (Booking's CDN
    // strips it): the page names the integration and mints an `aws-waf-token`.
    if lower.contains("awswafintegration")
        || lower.contains("aws-waf-token")
        || lower.contains("gokuprops")
        || lower.contains("awswaf.com")
    {
        found.push("aws-waf:challenge page".into());
        return Verdict {
            class: Class::WallProgram,
            wall: Some(Wall {
                vendor: "aws-waf".into(),
                marker: "challenge page".into(),
                asks: "WAF challenge",
            }),
            evidence: found,
        };
    }

    // A 2xx with no document and no other marker is a challenge stub, not an
    // incomplete page: the origin accepted the request and gave nothing back.
    let empty_body = evidence.body.trim().len() < 120
        && !evidence.body.contains("<script")
        && !evidence.body.contains("id=\"root\"");
    if empty_body
        && (200..300).contains(&evidence.status)
        && (evidence.status == 202 || evidence.status == 204 || evidence.body.trim().is_empty())
    {
        found.push("empty 2xx".into());
        return Verdict {
            class: Class::WallProgram,
            wall: Some(Wall {
                vendor: "unknown".into(),
                marker: "empty 2xx".into(),
                asks: "challenge",
            }),
            evidence: found,
        };
    }

    // A document whose only way forward is a login form needs a session, not a
    // program and not another identity.
    if lower.contains("type=\"password\"") || lower.contains("type=password") {
        found.push("login form".into());
        return Verdict {
            class: Class::WallState,
            wall: Some(Wall {
                vendor: "login".into(),
                marker: "password field".into(),
                asks: "sign-in",
            }),
            evidence: found,
        };
    }

    for signature in SIGNATURES {
        if let Some(marker) = signature
            .markers
            .iter()
            .find(|marker| lower.contains(&marker.to_ascii_lowercase()))
        {
            found.push(format!("{}:{}", signature.vendor, marker));
            if wall.is_none() {
                let (asks, class) = asks(signature.vendor, &lower);
                found.push(format!("asks:{asks}"));
                wall = Some(Wall {
                    vendor: signature.vendor.to_string(),
                    marker: (*marker).to_string(),
                    asks,
                });
                return Verdict {
                    class,
                    wall,
                    evidence: found,
                };
            }
        }
    }

    if evidence.status == 0 {
        return Verdict {
            class: Class::Network,
            wall: None,
            evidence: found,
        };
    }

    // Typographic apostrophes ("you’re human") read as ASCII ones; folded only
    // when the page has one, so an ordinary body is not copied again.
    let folded = if lower.contains(['\u{2019}', '\u{2018}', '\u{02bc}']) {
        std::borrow::Cow::Owned(crate::contract::fold_quotes(&lower))
    } else {
        std::borrow::Cow::Borrowed(lower.as_str())
    };
    if let Some(phrase) = ASKS_HUMAN.iter().find(|phrase| folded.contains(*phrase)) {
        found.push(format!("asks-human:{phrase}"));
        return Verdict {
            class: Class::WallInteraction,
            wall: Some(Wall {
                vendor: "unknown".into(),
                marker: (*phrase).to_string(),
                asks: "human verification",
            }),
            evidence: found,
        };
    }

    if matches!(evidence.status, 401 | 403 | 407 | 429 | 503) {
        if let Some(phrase) = REFUSALS.iter().find(|phrase| lower.contains(*phrase)) {
            found.push(format!("refusal:{phrase}"));
        }
        // Akamai's edge refuses a client before any sensor runs and names itself
        // only in headers: its server, and Bot Manager's cookies on the refusal.
        let akamai_edge = header("server")
            .map(|server| server.eq_ignore_ascii_case("akamaighost"))
            .unwrap_or(false);
        let bot_manager = evidence.headers.iter().any(|(key, value)| {
            key.eq_ignore_ascii_case("set-cookie")
                && ["_abck=", "bm_sz=", "bm_s=", "bm_so=", "ak_bmsc="]
                    .iter()
                    .any(|name| value.starts_with(name))
        });
        let wall = (akamai_edge && bot_manager).then(|| {
            found.push("akamai:edge refusal".into());
            Wall {
                vendor: "akamai".into(),
                marker: "AkamaiGHost + bot manager cookie".into(),
                asks: "edge refusal of this client",
            }
        });
        return Verdict {
            class: Class::WallIdentity,
            wall,
            evidence: found,
        };
    }

    if evidence.status >= 400 {
        return Verdict {
            class: Class::HttpError,
            wall: None,
            evidence: found,
        };
    }

    // A refusal served as a 2xx: a short page whose title or heading is the
    // site's "Access Denied" or interstitial. It is about this client, so it is
    // an identity wall, and another identity is asked. Akamai's refusal names
    // itself only by its reference link.
    if let Some(refusal) = crate::contract::refusal_page(evidence.body) {
        let vendor = if lower.contains("errors.edgesuite.net") || lower.contains("akamaighost") {
            "akamai"
        } else {
            "unknown"
        };
        found.push(format!("refusal:{refusal}"));
        return Verdict {
            class: Class::WallIdentity,
            wall: Some(Wall {
                vendor: vendor.into(),
                marker: refusal.to_string(),
                asks: "refusal of this client",
            }),
            evidence: found,
        };
    }

    // 2xx, no wall signature, contract failed: decide between a client-rendered
    // shell and an honest incomplete document. A shell is a document that ships
    // programs and little else.
    let scripts = lower.matches("<script").count();
    let text = text_chars(&lower);
    if scripts >= 3 && text < 400 {
        found.push(format!("shell:scripts={scripts},text={text}"));
        return Verdict {
            class: Class::Shell,
            wall: None,
            evidence: found,
        };
    }
    found.push(format!("grades:scripts={scripts},text={text}"));
    Verdict {
        class: Class::Incomplete,
        wall: None,
        evidence: found,
    }
}

/// Cheap text estimate for the shell test: strip tags, count the visible
/// characters. Only used to separate a shell from a document; the contract does
/// the real verification.
fn text_chars(lower: &str) -> usize {
    let mut count = 0;
    let mut inside = false;
    for character in lower.chars() {
        match character {
            '<' => inside = true,
            '>' => inside = false,
            _ if !inside && !character.is_whitespace() => count += 1,
            _ => {}
        }
    }
    count
}

#[cfg(test)]
mod tests {
    use super::*;

    fn class_of(status: u16, body: &str, content_ok: bool) -> Class {
        classify(Evidence {
            status,
            body,
            content_ok,
            headers: &[],
        })
        .class
    }

    #[test]
    fn a_pow_solve_page_is_its_challenge_and_an_article_about_one_is_not() {
        let page = r#"<script>const challengeId = "MpNBUn2OifdS_VNrp-IRTA";
const bits = 12;
async function submitSolve(nonce) { await fetch("/_pow/solve", {method: "POST"}); }</script>
<p>Cookies must be enabled</p>"#;
        let verdict = classify(Evidence {
            status: 203,
            body: page,
            content_ok: false,
            headers: &[],
        });
        assert_eq!(verdict.class, Class::WallContract);
        assert_eq!(verdict.wall.map(|w| w.vendor), Some("pow-solve".into()));
        // A page that only mentions the path, with no work stated, is read
        // as what it is.
        let article =
            "<article><p>The /_pow/solve endpoint takes a nonce and a challenge id.</p></article>";
        let verdict = classify(Evidence {
            status: 200,
            body: article,
            content_ok: true,
            headers: &[],
        });
        assert_eq!(verdict.class, Class::Content);
    }

    #[test]
    fn a_siteground_stub_is_its_challenge_whatever_the_contract_says() {
        let stub = r#"<html><head><meta http-equiv="refresh" content="0;/.well-known/sgcaptcha/?r=%2Fa"></meta></head></html>"#;
        let headers = [("sg-captcha".to_string(), "challenge".to_string())];
        let verdict = classify(Evidence {
            status: 202,
            body: stub,
            content_ok: true,
            headers: &headers,
        });
        assert_eq!(verdict.class, Class::WallContract);
        assert_eq!(verdict.wall.map(|w| w.vendor), Some("siteground".into()));
        // A SiteGround page that is not a challenge is content.
        let served = [("x-sg-cdn".to_string(), "1".to_string())];
        let verdict = classify(Evidence {
            status: 200,
            body: "<html><body><p>Article</p></body></html>",
            content_ok: true,
            headers: &served,
        });
        assert_eq!(verdict.class, Class::Content);
    }

    #[test]
    fn reddits_gate_is_the_pages_own_program() {
        let gate = "<html><head><title>Reddit</title></head><body><form hidden method=\"GET\" \
                    action=\"/r/rust/\"><input type=\"hidden\" name=\"solution\" />\
                    <input type=\"hidden\" name=\"js_challenge\" value=\"1\"/>\
                    <input type=\"hidden\" name=\"jsc_token\" value=\"ab12\"/></form></body></html>";
        let verdict = classify(Evidence {
            status: 200,
            body: gate,
            content_ok: false,
            headers: &[],
        });
        assert_eq!(verdict.class, Class::WallProgram);
        assert_eq!(
            verdict.wall.as_ref().map(|wall| wall.vendor.as_str()),
            Some("reddit")
        );
        // The same origin's block page names no program: it stays a refusal.
        let blocked = "<html><body>You've been blocked by network security.</body></html>";
        let verdict = classify(Evidence {
            status: 403,
            body: blocked,
            content_ok: false,
            headers: &[],
        });
        assert_eq!(verdict.class, Class::WallIdentity);
    }

    #[test]
    fn akamais_edge_refusal_is_named_from_its_headers() {
        let refusal = |headers: &[(String, String)]| {
            classify(Evidence {
                status: 403,
                body:
                    "<html><title>Error Page</title>Reference 0.2e87d817.1793934104.226b473d</html>",
                content_ok: false,
                headers,
            })
        };
        let akamai = [
            ("Server".to_string(), "AkamaiGHost".to_string()),
            ("Set-Cookie".to_string(), "bm_s=YAAQ; Path=/".to_string()),
        ];
        let verdict = refusal(&akamai);
        assert_eq!(verdict.class, Class::WallIdentity);
        assert_eq!(
            verdict.wall.map(|wall| wall.vendor),
            Some("akamai".to_string())
        );
        // An Akamai-served 403 without Bot Manager's cookie is the origin's own refusal.
        let verdict = refusal(&akamai[..1]);
        assert_eq!(verdict.class, Class::WallIdentity);
        assert!(verdict.wall.is_none());
    }

    #[test]
    fn content_wins_over_every_marker() {
        // A page that mentions a captcha but satisfies its contract is content.
        assert_eq!(
            class_of(200, "<title>t</title> captcha recaptcha/api.js", true),
            Class::Content
        );
    }

    #[test]
    fn managed_challenge_is_the_page_program() {
        let body = "<script>window._cf_chl_opt={cvId:'3'}</script>";
        assert_eq!(class_of(403, body, false), Class::WallProgram);
    }

    #[test]
    fn legacy_jschl_is_a_native_contract() {
        // A real legacy challenge carries the Cloudflare markers and the jschl form.
        let body = "<script>window.__cf_chl_opt={}</script><form id='challenge-form'><input name='jschl_vc'></form>";
        assert_eq!(class_of(403, body, false), Class::WallContract);
    }

    #[test]
    fn widget_beats_host_vendor() {
        // Cloudflare hosts Turnstile; the human widget is what the page asks for.
        let body = "<script src='https://challenges.cloudflare.com/turnstile/v0/api.js'></script>";
        assert_eq!(class_of(403, body, false), Class::WallInteraction);
    }

    #[test]
    fn datadome_slider_is_interaction() {
        let body = "<iframe src='https://geo.captcha-delivery.com/captcha/?x'>";
        assert_eq!(class_of(403, body, false), Class::WallInteraction);
    }

    #[test]
    fn refusal_without_a_page_is_identity() {
        assert_eq!(class_of(403, "Forbidden", false), Class::WallIdentity);
        assert_eq!(
            class_of(429, "Too Many Requests", false),
            Class::WallIdentity
        );
    }

    #[test]
    fn a_program_driven_document_is_a_shell() {
        let body =
            "<div id=\"root\"></div><script>a()</script><script>b()</script><script>c()</script>";
        assert_eq!(class_of(200, body, false), Class::Shell);
    }

    #[test]
    fn a_refusal_served_as_200_is_an_identity_wall() {
        let akamai = "<HTML><HEAD><TITLE>Access Denied</TITLE></HEAD><BODY><H1>Access Denied</H1>\
            You don't have permission to access this server.<P>Reference #18.a5f4\
            <P>https://errors.edgesuite.net/18.a5f4</BODY></HTML>";
        let verdict = classify(Evidence {
            status: 200,
            body: akamai,
            content_ok: false,
            headers: &[],
        });
        assert_eq!(verdict.class, Class::WallIdentity);
        assert_eq!(
            verdict.wall.map(|wall| wall.vendor),
            Some("akamai".to_string())
        );
        // A typographic apostrophe does not hide a human ask.
        let hold = "<title>Robot or human?</title><p>Activate and hold the button to confirm that you\u{2019}re human.</p>";
        assert_eq!(class_of(200, hold, false), Class::WallInteraction);
        // An ordinary short page that failed its contract is not a refusal.
        assert_eq!(
            class_of(200, "<title>Docs</title><p>Short.</p>", false),
            Class::Incomplete
        );
    }

    #[test]
    fn transport_failure_is_network() {
        assert_eq!(class_of(0, "", false), Class::Network);
    }
}
