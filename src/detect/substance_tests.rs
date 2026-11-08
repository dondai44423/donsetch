// Shapes reduced from live captures (2026-11-08). Each "not content" case
// has a "content" neighbour that differs only in what the verdict must
// read: the guard against the opposite error.

use super::*;

fn verdict(html: &str) -> Verdict {
    assess(&Html::parse_document(html), "https://example.test/page").verdict
}

fn verdict_at(html: &str, url: &str) -> Verdict {
    assess(&Html::parse_document(html), url).verdict
}

const FOOTER: &str = r#"<footer><a href="/about">About</a><a href="/help">Help</a>
<a href="/terms">Terms of Service</a><a href="/privacy">Privacy Policy</a>
<p>© 2026 Example Inc. All rights reserved.</p></footer>"#;

// ------------------------------------------------------------ not content

#[test]
fn an_empty_app_mount_inside_a_static_frame_is_not_content() {
    // artstation /artwork/: the app root is empty; the nav, a sign-up
    // modal and currency prompts sit outside it.
    let html = format!(
        r#"<body><app-root></app-root>
        <nav><a href="/">Gallery</a><a href="/blogs">Blogs</a><a href="/learning">Learning</a>
        <a href="/jobs">Job Listings</a><a href="/studios">Hiring Studios</a></nav>
        <div class="modal"><h4>Sign Up</h4><p>Join the leading showcase platform for art and design.</p>
        <a href="/signup">Sign Up with Epic Games</a><p>Already have an account?</p><a href="/in">Sign In here</a></div>
        <div>It looks like you're using ArtStation from Europe. Would you like to change the currency to Euros (€)?</div>
        {FOOTER}</body>"#
    );
    assert!(!verdict(&html).is_content());
}

#[test]
fn navigation_regions_alone_are_a_shell() {
    // semrush /analytics/overview/: header, nav and footer, an empty main.
    let html = format!(
        r#"<body><header><a href="/pricing">Pricing</a><a href="/features">Features</a>
        <a href="/login">Log In</a><a href="/signup">Sign Up</a></header>
        <nav><ul><li><a href="/a">Site Audit</a></li><li><a href="/b">Position Tracking</a></li>
        <li><a href="/c">Keyword Magic Tool</a></li><li><a href="/d">Backlink Gap</a></li></ul></nav>
        <main><h1>Domain Overview</h1></main>{FOOTER}</body>"#
    );
    assert_eq!(verdict(&html), Verdict::Shell);
}

#[test]
fn a_site_blurb_in_a_footer_region_does_not_make_a_login_landing_content() {
    // pixiv /en/ logged out: CSS-module class names, no semantic elements.
    let html = r#"<body><div id="__next"><div class="NoLoginIndex_root__t_a9b">
        <div><a href="/r">Rankings</a></div><div><a href="/c">Contests</a></div>
        <div>Log in with your pixiv account</div><div><a href="/s">Create account</a><a href="/l">Login</a></div>
        <div><button>Continue with Google</button></div><div><button>Continue with Apple</button></div>
        <div>This site is protected by reCAPTCHA.</div>
        <div class="FooterBar_footerContainer__FdGTP"><div>About pixiv</div>
        <span class="FooterBar_mottoDesc__wUv6T">pixiv is one of the world's largest art communities, where you can enjoy browsing and posting illustrations, manga, and novels.</span>
        <div><a href="/comic">pixiv COMIC</a></div><div><a href="/fanbox">pixivFANBOX</a></div></div>
        </div></div></body>"#;
    assert!(!verdict(html).is_content());
}

#[test]
fn a_loading_placeholder_with_frame_is_loading() {
    // speedtest.net: the app shows "Finding optimal server..." until scripts run.
    let html = format!(
        r#"<body><div id="root"><header><a href="/">Speedtest</a><a href="/apps">Apps</a></header>
        <h1>Speedtest by Ookla</h1><div>Results Settings</div>
        <h3>Finding provider</h3><h3>Finding optimal server...</h3></div>{FOOTER}</body>"#
    );
    assert_eq!(verdict(&html), Verdict::Loading);
}

#[test]
fn a_sign_in_form_is_an_auth_gate() {
    // accounts.google.com: one heading, a form, a language picker.
    let html = r#"<body><div>Loading</div><main><h1>Sign in</h1><div>to continue to Google Calendar</div>
        <form><input type="email"><div>Email or phone</div><div>Not your computer? Use Guest mode to sign in privately.</div>
        <button>Next</button></form>
        <ul><li>Afrikaans</li><li>Deutsch</li><li>English (United States)</li><li>Español (España)</li></ul></main></body>"#;
    assert_eq!(verdict(html), Verdict::AuthGate);
}

#[test]
fn a_cjk_sign_in_page_is_an_auth_gate() {
    // zhihu.com/signin: QR and SMS sign-in inside <main>, licence lines below.
    let html = r#"<body><div id="root"><main>
        <div>打开知乎App</div><div>在「我的页」右上角打开扫一扫</div><div>其他扫码方式：微信</div>
        <div><button>验证码登录</button><button>密码登录</button></div><div>获取短信验证码</div>
        <div><button>登录/注册</button></div><div>未注册手机验证后自动登录，注册即代表同意 《知乎协议》 《隐私保护指引》</div>
        </main><footer>© 2026 知乎 京ICP证110745号</footer></div></body>"#;
    assert_eq!(verdict(html), Verdict::AuthGate);
}

#[test]
fn an_unsupported_browser_notice_is_a_notice_not_content() {
    // soundcloud.com/discover: two full sentences, both about the browser.
    let html = r#"<body><div id="app"><div class="notice">
        <p>Your current browser isn't compatible with SoundCloud.</p>
        <p>Please download one of our supported browsers. Need help?</p>
        <p>Sorry! Something went wrong</p></div></div></body>"#;
    assert_eq!(verdict(html), Verdict::Notice);
    let page = assess(
        &Html::parse_document(html),
        "https://soundcloud.com/discover",
    );
    assert!(page.notice.unwrap().contains("isn't compatible"));
}

#[test]
fn a_region_block_is_a_notice() {
    let html = format!(
        r#"<body><div id="root"><h2>Sorry, Disney+ is not available in your region.</h2>
        <a href="/login">Already a subscriber? Sign In</a></div>{FOOTER}</body>"#
    );
    assert_eq!(verdict(&html), Verdict::Notice);
}

#[test]
fn a_footer_in_a_script_heavy_app_is_a_shell() {
    // youtube /@channel/videos: localized footer links only, megabytes of script.
    let script = "x".repeat(20_000);
    let html = format!(
        r#"<html><head><script>{script}</script></head><body><ytd-app>
        <a slot="guide-links-primary" href="/about">बारेमा</a><a slot="guide-links-primary" href="/press">प्रेस</a>
        <a slot="guide-links-primary" href="/copyright">प्रतिलिपि अधिकार</a>
        <div>© 2026 Google LLC</div></ytd-app></body></html>"#
    );
    assert!(!verdict(&html).is_content());
}

#[test]
fn a_bare_app_document_is_empty() {
    let html = r#"<html><head><script src="/assets/index-3f2a.js" type="module"></script></head>
        <body><div id="root"></div></body></html>"#;
    assert_eq!(verdict(html), Verdict::Empty);
}

#[test]
fn a_search_box_echo_is_not_content() {
    // wolframalpha /input?i=2+2: the only text is the query, echoed.
    let script = "x".repeat(5_000);
    let html = format!(
        r#"<html><head><script>{script}</script></head><body><div id="root"><div>2+2</div></div></body></html>"#
    );
    assert!(!verdict_at(&html, "https://www.wolframalpha.com/input?i=2%2B2").is_content());
}

#[test]
fn a_noscript_redirect_is_reported() {
    // duckduckgo: script-less clients are sent to the HTML site.
    let html = r#"<html><head><noscript><meta http-equiv="refresh" content="0;URL=/html?q=rust"></noscript></head>
        <body><div id="app"></div><noscript><div class="msg">You are being redirected to the non-JavaScript site.</div></noscript></body></html>"#;
    let page = assess(
        &Html::parse_document(html),
        "https://duckduckgo.com/?q=rust",
    );
    assert!(!page.verdict.is_content());
    assert_eq!(
        page.noscript_redirect.as_deref(),
        Some("https://duckduckgo.com/html?q=rust")
    );
}

// ---------------------------------------------------------------- content

#[test]
fn one_sentence_is_content() {
    // example.com: 1 paragraph, nothing else.
    let html = r#"<body><div><h1>Example Domain</h1><p>This domain is for use in documentation examples without needing permission.</p>
        <p><a href="https://iana.org/domains/example">Learn more</a></p></div></body>"#;
    assert_eq!(verdict(html), Verdict::Content);
}

#[test]
fn a_one_sentence_page_with_a_footer_is_content() {
    let html = format!(r#"<body><main><p>Software engineer in Berlin.</p></main>{FOOTER}</body>"#);
    assert_eq!(verdict(&html), Verdict::Content);
}

#[test]
fn a_noscript_answer_is_content() {
    // isitchristmas.com and hasthelargehadroncolliderdestroyedtheworldyet.com
    // answer script-less clients in <noscript>.
    let html = r#"<html><head><script>var x=1;</script></head><body>
        <a id="answer" href="/"><noscript>NO</noscript></a>
        <div id="links"><a href="https://bsky.app/profile/konklone.com">by @konklone</a></div></body></html>"#;
    assert_eq!(verdict(html), Verdict::Content);
    let html = r#"<body><div id="main"><script>document.write("NOPE.");</script><noscript>NOPE.</noscript></div></body>"#;
    let page = assess(&Html::parse_document(html), "https://example.test/");
    assert_eq!(page.verdict, Verdict::Content);
    assert_eq!(page.noscript_text, vec!["NOPE.".to_string()]);
}

#[test]
fn a_short_post_with_counts_and_sign_in_prompts_is_content() {
    // x.com/jack/status/20: four words, a date and counts, sign-in prompts around.
    let html = r#"<body><div id="react-root"><main>
        <h2>Post</h2><div><a href="/login">Log in</a><a href="/signup">Sign up</a></div>
        <article><div><a href="/jack">jack</a><a href="/jack">@jack</a></div>
        <div>just setting up my twttr</div><div><a href="/jack/status/20">8:50 PM · Mar 21, 2006</a></div>
        <div>19K</div><div>134K</div></article>
        <h2>Log in or sign up for X</h2><div>See what’s happening and join the conversation</div>
        <button>Continue with Google</button></main></div></body>"#;
    assert_eq!(verdict(html), Verdict::Content);
}

#[test]
fn a_profile_table_is_content() {
    // news.ycombinator.com/user?id=pg: a few rows of label + value.
    let html = r#"<body><table><tr><td>user:</td><td>pg</td></tr><tr><td>created:</td><td>October 9, 2006</td></tr>
        <tr><td>karma:</td><td>157316</td></tr></table></body>"#;
    assert_eq!(verdict(html), Verdict::Content);
}

#[test]
fn a_directory_inside_an_article_classed_footer_is_content() {
    // craigslist /about/sites: city lists in <article class="flexed-footer">.
    let html = format!(
        r#"<body><article class="page-container flexed-footer"><section class="body">
        <h1>craigslist: find craigslist classifieds by city</h1>
        <ul><li><a href="//auburn.craigslist.org">auburn</a></li><li><a href="//bham.craigslist.org">birmingham</a></li>
        <li><a href="//dothan.craigslist.org">dothan</a></li><li><a href="//gadsden.craigslist.org">gadsden-anniston</a></li></ul>
        </section></article>{FOOTER}</body>"#
    );
    assert_eq!(verdict(&html), Verdict::Content);
}

#[test]
fn a_page_of_labels_without_frame_evidence_is_not_failed_on_a_guess() {
    let html = r#"<body><h1>Favourite places</h1><ul><li>Lisbon</li><li>Kyoto</li></ul></body>"#;
    assert_eq!(verdict(html), Verdict::Content);
}

#[test]
fn a_status_list_is_content() {
    let html = format!(
        r#"<body><div id="__next"><h1>All Systems Operational</h1>
        <div>Git Operations</div><div>Normal</div><div>API Requests</div><div>Normal</div>
        <div>Uptime 99.98%</div></div>{FOOTER}</body>"#
    );
    assert_eq!(verdict(&html), Verdict::Content);
}

#[test]
fn a_code_block_is_content() {
    let html = format!(
        r#"<body><nav><a href="/">Home</a><a href="/docs">Docs</a></nav><pre>cargo add serde --features derive</pre>{FOOTER}</body>"#
    );
    assert_eq!(verdict(&html), Verdict::Content);
}

#[test]
fn an_article_about_signing_in_is_content() {
    let html = r#"<body><main><h1>How to sign in to your account</h1>
        <p>To sign in, open the app and enter the email address you registered with.</p>
        <p>If you forgot your password, choose reset password and follow the link we send you.</p>
        <p>Two-step verification adds a code from your phone to every new sign in.</p></main></body>"#;
    assert_eq!(verdict(html), Verdict::Content);
}

#[test]
fn a_cjk_paragraph_is_content() {
    let html = r#"<body><div id="app"><p>今天北京的天气晴朗，最高气温二十五度，适合户外活动。</p></div></body>"#;
    assert_eq!(verdict(html), Verdict::Content);
}

#[test]
fn deep_nesting_does_not_overflow() {
    let html = format!(
        "<body>{}<p>Deep content sits here in a full sentence.</p>{}</body>",
        "<div>".repeat(50_000),
        "</div>".repeat(50_000)
    );
    // html5ever caps nesting on its own; the walk must not recurse either way.
    let _ = verdict(&html);
}

#[test]
fn data_tokens() {
    assert!(has_data("19K"));
    assert!(has_data("30% off hotels"));
    assert!(has_data("$9.99"));
    assert!(has_data("8:50 PM"));
    assert!(has_data("Uptime 99.98%"));
    assert!(!has_data("© 2026 Example"));
    assert!(!has_data("VISIONS2026"));
    assert!(!has_data("88248ae2-086d-486b"));
    assert!(!has_data("2+2"));
}

#[test]
fn sentences_and_labels() {
    assert!(is_sentence(
        "This domain is for use in documentation examples."
    ));
    assert!(is_sentence("Software engineer in Berlin."));
    assert!(!is_sentence("Contact us About us Blog"));
    assert!(!is_sentence("Prepare to Talk to Your ISP"));
    assert!(!is_sentence("Finding optimal server..."));
    assert_eq!(label_kind("English (United States)"), Label::Lang);
    assert_eq!(label_kind("Terms of Use"), Label::Legal);
    assert_eq!(label_kind("About Us"), Label::Nav);
    assert_eq!(label_kind("Keyword Magic Tool"), Label::Entity);
}

#[test]
fn class_tokens_split_css_module_names() {
    assert_eq!(
        tokens("FooterBar_mottoDesc__wUv6T sc-73d9"),
        vec!["footer", "bar", "motto", "desc", "w", "uv6t", "sc", "73d9"]
    );
}

#[test]
fn a_redirect_to_a_sign_in_endpoint_is_a_login_wall() {
    assert!(login_redirect(
        "https://calendar.google.com/",
        "https://accounts.google.com/v3/signin/identifier?continue=https://calendar.google.com/calendar/u/0/r"
    ));
    assert!(login_redirect(
        "https://www.zhihu.com/",
        "https://www.zhihu.com/signin?next=%2F"
    ));
    assert!(login_redirect(
        "https://www.netflix.com/browse",
        "https://www.netflix.com/np/login?nextpage=https%3A%2F%2Fwww.netflix.com%2Fbrowse"
    ));
    assert!(login_redirect(
        "https://www.facebook.com/groups/x",
        "https://www.facebook.com/login.php?next=x"
    ));
    assert!(login_redirect(
        "https://www.linkedin.com/in/x",
        "https://www.linkedin.com/authwall?trk=x"
    ));
}

#[test]
fn a_requested_login_page_or_a_lookalike_path_is_not_a_login_wall() {
    assert!(!login_redirect(
        "https://github.com/login",
        "https://github.com/login"
    ));
    assert!(!login_redirect(
        "https://example.com/a",
        "https://example.com/author/login-tips"
    ));
    assert!(!login_redirect(
        "https://example.com/blog/x",
        "https://example.com/blog/how-to-login"
    ));
    assert!(!login_redirect(
        "https://example.com/",
        "https://example.com/en/"
    ));
    assert!(!login_redirect("not a url", "https://example.com/login"));
}

#[test]
fn leaked_framework_identifiers_are_not_content() {
    // facebook's logged-out document: React server identifiers as text nodes.
    let script = "x".repeat(40_000);
    let html = format!(
        r#"<html><head><script>{script}</script></head><body><div>
        <h1>is_latency_sensitive_broadcast</h1><div>FDSTooltipDef</div><div>[["low","normal"]]</div>
        <div>CometNotificationsDropdownQuery</div></div></body></html>"#
    );
    assert!(!verdict(&html).is_content());
    assert!(is_identifier("is_latency_sensitive_broadcast"));
    assert!(!is_identifier("GitHub"));
    assert!(!is_identifier("well-known"));
}

#[test]
fn an_empty_language_picker_names_its_english_alternate() {
    // nohello.net: an empty body whose module script sends the browser to
    // a language path.
    let html = r#"<html><head><link rel="alternate" hreflang="de" href="https://nohello.net/de">
        <link rel="alternate" hreflang="en" href="https://nohello.net/en"></head>
        <body><script type="module">window.location = pick();</script></body></html>"#;
    let page = assess(&Html::parse_document(html), "https://nohello.net/");
    assert_eq!(page.verdict, Verdict::Empty);
    assert_eq!(page.language_hop.as_deref(), Some("https://nohello.net/en"));
    // A page with content never hops.
    let html = r#"<html><head><link rel="alternate" hreflang="en" href="https://nohello.net/en"></head>
        <body><p>Please do not say just hello in chat, ask your question.</p></body></html>"#;
    assert!(
        assess(&Html::parse_document(html), "https://nohello.net/")
            .language_hop
            .is_none()
    );
}

#[test]
fn an_article_inside_a_page_wide_header_is_content() {
    // holisticai.com /blog/: a Webflow article inside
    // <header class="section-hero-header">, a short real footer below.
    let html = format!(
        r#"<body class="hai-body"><nav><a href="/">Blog</a><a href="/demo">Get a demo</a></nav>
        <header class="section-hero-header"><div class="lr_blog-child-content"><div class="lr-rich w-richtext">
        <h1>Penalties of the EU AI Act: The High Cost of Non-Compliance</h1>
        <p>Non-compliance with the prohibitions can result in fines of up to <strong>35 million EUR</strong> or 7% of worldwide annual turnover.</p>
        <p>Other obligations carry fines of up to <strong>15 million EUR</strong> or 3% of turnover, whichever is higher.</p>
        <p>Supplying incorrect information to authorities can cost up to 7.5 million EUR or 1% of turnover.</p>
        </div></div></header>{FOOTER}</body>"#
    );
    assert_eq!(verdict(&html), Verdict::Content);
}

#[test]
fn a_mega_menu_holding_most_text_is_still_frame() {
    let items: String = (0..40)
        .map(|i| format!(r#"<li><a href="/t{i}">Keyword Tool {i}</a></li>"#))
        .collect();
    let html = format!(
        r#"<body><nav><ul>{items}</ul></nav><main><h1>Overview</h1></main>{FOOTER}</body>"#
    );
    assert!(!verdict(&html).is_content());
}

#[test]
fn a_page_wide_banner_without_prose_is_still_frame() {
    // huggingface.co/spaces/...: the banner holds the Space name, a like
    // count and a status; the app itself loads into an iframe.
    let html = r#"<body><header class="space-header"><h1><a href="/spaces">Spaces</a>
        <a href="/org">open-llm-leaderboard</a> / <a href="/s">open_llm_leaderboard</a>
        <button>Like</button><span>14.2k</span><span>Running on CPU Upgrade</span></h1>
        <div><a href="/app">App</a><a href="/files">Files</a><a href="/c">Community</a><span>1168</span></div></header>
        <main><div>Fetching metadata from the HF Docker repository...</div><div>Refreshing</div><iframe src="https://x.hf.space"></iframe></main></body>"#;
    assert!(!verdict(html).is_content());
}

#[test]
fn recovered_state_is_judged_by_its_substance() {
    // A short product record: a sentence and a price, far under any size bar.
    assert!(assess_lines(["# Trail Runner 2", "Light shoe for wet rock.", "$129.00"]).is_content());
    // One figure is enough.
    assert!(assess_lines(["Population", "8,804,190"]).is_content());
    // UI strings from an app bundle are not content, however many.
    assert!(
        !assess_lines([
            "Loading...",
            "Sign in",
            "Log in",
            "Create account",
            "Privacy Policy",
            "Terms of Service",
        ])
        .is_content()
    );
    // Machine identifiers recover nothing.
    assert!(!assess_lines(["is_latency_sensitive_broadcast", "FDSTooltipDef"]).is_content());
    assert_eq!(assess_lines(Vec::<&str>::new()), Verdict::Empty);
}

// Slack and Microsoft Teams (2026-11-08) answer a browser they will not
// serve with a page that says so; the apology and the list of browsers to
// install under that headline are the notice, not content.
#[test]
fn a_page_refusing_the_readers_browser_is_a_notice() {
    let slack = format!(
        r#"<body><nav><a href="/product">Product</a><a href="/pricing">Pricing</a><a href="/signin">Sign in</a></nav>
        <h1>Please change browsers to use Slack</h1>
        <p>We are no longer supporting this browser, so you’ll need to switch to one of our supported browsers to keep using Slack. We know this can be a pain, and we’re sorry for asking you to do it.</p>
        <p>For questions or support, go to the Slack Help Center.</p><a href="/help">Learn More</a>{FOOTER}</body>"#
    );
    assert_eq!(verdict(&slack), Verdict::Notice);
    let teams = r#"<body><h1>Hmm. Your browser version isn't supported. Quickest solution? Download the desktop app.</h1>
        <a href="/download">Download Microsoft Teams</a><p>Here are the supported browser versions.</p>
        <p>Edge</p><span>(latest three versions)</span><a href="/edge">Install for free</a>
        <p>Chrome</p><span>(latest three versions)</span><a href="/chrome">Install for free</a>
        <p>Safari</p><span>(latest two versions)</span><a href="/safari">Install for free</a></body>"#;
    assert_eq!(verdict(teams), Verdict::Notice);
}

#[test]
fn a_page_about_browsers_is_content() {
    // A help page that lists the supported browsers names them; it refuses
    // nobody.
    let docs = r#"<body><main><h1>Supported browsers</h1>
        <p>Slack supports the latest versions of Chrome, Firefox, Safari and Edge on desktop computers.</p>
        <ul><li>Chrome 140 or later</li><li>Firefox 135 or later</li><li>Safari 18 or later</li></ul></main></body>"#;
    assert_eq!(verdict(docs), Verdict::Content);
    // An article whose headline names the refusal keeps its sections.
    let article = r#"<body><main><h1>How to fix "your browser is not supported" errors</h1>
        <p>Some sites check the browser version before they load.</p>
        <h2>Update the browser</h2><p>Open the settings menu and choose About to install the latest version, then restart.</p>
        <h2>Clear the cache</h2><p>A stale cached copy of the site can keep showing the old warning after an update.</p></main></body>"#;
    assert_eq!(verdict(article), Verdict::Content);
    let imperative = r#"<body><main><h1>Update your browser to keep your account safe</h1>
        <p>Browsers patch security holes in every release, and an old one leaves those holes open.</p>
        <h2>Why updates matter</h2><p>Attackers scan for outdated browsers because the flaws are already public.</p></main></body>"#;
    assert_eq!(verdict(imperative), Verdict::Content);
}

// federalregister.gov and openreview.net served to a flagged client
// (2026-11-08): a headline that is the check, over the explanation of it.
#[test]
fn a_bot_check_or_access_refusal_is_a_notice() {
    let federal = r#"<body><header><a href="/my/sign_in">Sign in</a><a href="/my/sign_up">Sign up</a><a href="/my/clippings">My Clipboard</a></header><main><h1>Request Access</h1>
        <p>Due to aggressive automated scraping of FederalRegister.gov and eCFR.gov, programmatic access to these sites is limited to access to our extensive developer APIs.</p>
        <p>Your request has been flagged as potentially automated. If you are human user receiving this message, please complete the CAPTCHA (bot test) below and click "Request Access".</p>
        <form><button>Request Access</button></form></main><footer><p>An official website of the United States government</p></footer></body>"#;
    let openreview = r#"<body><nav><a href="/">OpenReview.net</a></nav><main><h1>Verifying your browser</h1>
        <h2>Complete the check below to continue to OpenReview</h2><p>Please complete the verification above.</p>
        <p>Have an OpenReview account? <a href="/login">Sign in</a> to skip this check.</p></main></body>"#;
    for (page, quote) in [
        (federal, "Request Access"),
        (openreview, "Verifying your browser"),
    ] {
        let read = assess(&Html::parse_document(page), "https://example.test/");
        assert_eq!(read.verdict, Verdict::Notice, "{page}");
        assert_eq!(read.notice.as_deref(), Some(quote));
    }
    for headline in [
        "Just a moment...",
        "Checking your browser before accessing example.com",
        "Access to this page has been denied",
        "Are you a robot?",
        "Sorry, you have been blocked",
    ] {
        let page = format!(
            "<body><h1>{headline}</h1><p>This process is automatic and your browser will redirect to the requested content shortly.</p></body>"
        );
        // A notice, or (for "Just a moment...") a loading placeholder.
        let read = verdict(&page);
        assert!(
            matches!(read, Verdict::Notice | Verdict::Loading),
            "{headline}: {read:?}"
        );
    }
}

#[test]
fn an_article_about_bot_checks_is_content() {
    for article in [
        r#"<body><main><h1>Access denied: what a 403 error means</h1>
        <p>A 403 response says the server understood the request and refuses to authorize it, usually for lack of permission.</p></main></body>"#,
        r#"<body><main><h1>Security check-in tips for busy airports</h1>
        <p>Arrive two hours early, keep liquids in a clear bag and take laptops out before the scanner.</p></main></body>"#,
        r#"<body><main><h1>Verifying your browser's extensions before a release</h1>
        <p>Load the unpacked build, open the extensions page and confirm that the permissions match the manifest.</p></main></body>"#,
    ] {
        assert_eq!(verdict(article), Verdict::Content, "{article}");
    }
}

// heineken.com served to a reader without the age cookie (2026-11-08): the
// headline asks the birth date over a country list and a province picker.
#[test]
fn an_age_gate_is_a_notice() {
    let heineken = format!(
        r#"<body><div class="gate"><h1><span>Please tell us:<br>When were you born?</span></h1>
        <form><select><option>Afghanistan</option><option>Albania</option><option>Algeria</option></select>
        <input placeholder="DD"><input placeholder="MM"><input placeholder="YYYY"></form>
        <p>Please select a province</p>
        <p>Always drink responsibly. Always surf the web responsibly too.</p>
        <p>You have to be of legal drinking age to enter this site.</p></div>{FOOTER}</body>"#
    );
    let page = assess(&Html::parse_document(&heineken), "https://example.test/");
    assert_eq!(page.verdict, Verdict::Notice);
    assert_eq!(
        page.notice.as_deref(),
        Some("Please tell us: When were you born?")
    );
    for headline in [
        "Are you of legal drinking age?",
        "Are you 21 or older?",
        "Are you over 18 years old?",
        "You must be 18 or older to enter this site",
        "Please enter your date of birth",
        "Age verification",
    ] {
        let gate = format!(
            r#"<body><h1>{headline}</h1><p>By entering this site you agree to our terms of use and privacy policy.</p><button>Yes</button><button>No</button></body>"#
        );
        assert_eq!(verdict(&gate), Verdict::Notice, "{headline}");
    }
}

#[test]
fn an_article_about_age_is_content() {
    for article in [
        // The headline goes on past the question.
        r#"<body><main><h1>Are you over 50? Here's what to know about the shingles vaccine</h1>
        <p>Adults over fifty are offered two doses, given two to six months apart, and the protection lasts for years.</p>
        <p>Side effects are usually mild: a sore arm, tiredness or a headache for a day or two.</p></main></body>"#,
        // A question headline over sections.
        r#"<body><main><h1>When were you born?</h1><p>Your birth year shapes the music you grew up with.</p>
        <h2>The eighties</h2><p>Synthesizers moved from studios into bedrooms and every chart had a drum machine.</p>
        <h2>The nineties</h2><p>Grunge and hip hop split the radio between two very different cities.</p></main></body>"#,
        // A headline about age checks is not one.
        r#"<body><main><h1>Age verification laws are spreading across the US</h1>
        <p>Eight states now require adult sites to check a visitor's age with an ID or a third-party service.</p></main></body>"#,
    ] {
        assert_eq!(verdict(article), Verdict::Content, "{article}");
    }
}

// claude.ai/new served to a reader with no session (2026-11-08): the only
// text is the statutory Korean business footer, outside any footer element.
#[test]
fn a_korean_business_footer_alone_is_not_content() {
    let shell = r#"<body><div id="root"></div><div class="legal">
        <div>Anthropic PBC | 사업자등록번호 86-1969045</div>
        <div>Anthropic PBC | 사업자등록번호 86-1969045 (미국 델라웨어) | 다리오 아모데이</div>
        <div>통신판매업 신고: 2025-공정-0020호 | 사업자정보확인 | 호스팅제공자: AWS Cloud Services | 이용약관</div>
        <div>주소: 500 Howard Street, San Francisco, California U.S. | 대표전화: 1-415-505-5563 | support@anthropic.com</div>
        </div></body>"#;
    assert_ne!(verdict(shell), Verdict::Content);
    let page = shell.replace(
        r#"<div id="root"></div>"#,
        "<main><h1>서비스 안내</h1><p>이 서비스는 연구와 글쓰기를 돕는 인공지능 도우미입니다. 질문을 입력하면 답변을 받을 수 있습니다.</p></main>",
    );
    assert_eq!(verdict(&page), Verdict::Content);
}
