//! Substance: does an HTML document carry content, or only the frame
//! around content that never arrived?
//!
//! A client-rendered page answers a script-less read with its frame:
//! navigation, sign-in prompts, legal lines, a loading placeholder, an
//! "unsupported browser" notice. That text is not the page, and serving it
//! as content is the false positive this module exists to stop. The
//! opposite error matters as much: a short page with real content (a
//! one-line status, a profile card, a "NO" answer) must never fail for
//! being short.
//!
//! So the verdict is never a size. The document is read once into units
//! (one per block of visible text, with the structure around it: a
//! navigation or footer region, a sign-in form, a heading, a link, a table
//! row, an app mount point), each unit is classified, and:
//!
//! - **Content** when the page carries one unit of substance: a sentence
//!   outside the frame, a line that carries data (a count, a price, a time,
//!   a percentage), a table row, a code block, a `<noscript>` answer the
//!   site serves to clients without scripts, or a listing of three or more
//!   named things in the page's main region.
//! - **Not content** only with positive evidence besides: a sign-in gate,
//!   a loading placeholder, a notice standing in for the page, frame units
//!   that outnumber everything else, or a client-app document with nothing
//!   but a stray label or two. Each verdict names which.
//! - **Content** again when there is neither substance nor evidence: a
//!   page of bare names or labels is not failed on a guess.
//!
//! Failure modes, stated: a false "not content" needs a page whose only
//! prose sits inside a navigation, footer or sign-in region, or reads as a
//! legal, cookie, locale, loading or error line, and that has no data, no
//! table and no code. A false "content" needs frame text the lexicons do
//! not know phrased as an ordinary sentence outside any marked region; the
//! fetch layer's wall and challenge checks still run on it.

use std::collections::HashSet;
use std::sync::LazyLock;

use regex::Regex;
use scraper::{Html, Node};

/// What the document is, for a reader.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub enum Verdict {
    /// The page carries content.
    Content,
    /// No visible text at all.
    Empty,
    /// The frame of a client-rendered page: navigation, legal lines and
    /// labels, with the content left to scripts.
    Shell,
    /// A sign-in page or panel standing in for the content.
    AuthGate,
    /// A loading placeholder ("Loading...", "Finding optimal server...").
    Loading,
    /// A notice standing in for the page: an unsupported-browser or
    /// scripts-required message, an error, a region block, a challenge.
    Notice,
}

impl Verdict {
    pub fn is_content(self) -> bool {
        matches!(self, Verdict::Content)
    }
}

/// The assessment of one document.
#[derive(Debug, Clone)]
pub struct Page {
    pub verdict: Verdict,
    /// Units of substance found (sentences, data lines, rows, code, ...).
    pub substance: usize,
    /// The first notice the page shows in place of content, trimmed: the
    /// honest thing to quote when the read fails.
    pub notice: Option<String>,
    /// A redirect the page declares for clients without scripts
    /// (`<noscript><meta http-equiv="refresh">`), resolved against the
    /// page URL.
    pub noscript_redirect: Option<String>,
    /// Text a `<noscript>` element serves as the page's answer to clients
    /// without scripts (not a scripts-required notice).
    pub noscript_text: Vec<String>,
    /// For a document with no visible text: the version of the page it
    /// declares for an English reader (`hreflang` x-default, then en), where
    /// a script would have sent a browser (nohello.net's language picker).
    pub language_hop: Option<String>,
}

/// Parse and assess an HTML body.
pub fn assess_html(body: &[u8], url: &str) -> Page {
    let text = String::from_utf8_lossy(body);
    let doc = Html::parse_document(&text);
    assess(&doc, url)
}

// ---------------------------------------------------------------- lexicons

/// Legal, cookie and trademark lines: a footer's sentences.
const LEGAL: &[&str] = &[
    "©",
    "copyright",
    "all rights reserved",
    "privacy policy",
    "privacy notice",
    "privacy statement",
    "terms of service",
    "terms of use",
    "terms and conditions",
    "terms & conditions",
    "cookie",
    "do not sell",
    "privacy choices",
    "legal notice",
    "impressum",
    "trademark",
    "recaptcha",
    "updating these terms",
    "new terms",
    "icp备",
    "icp证",
    "公网安备",
    "版权所有",
    "利用規約",
    // A Korean site's statutory business footer: registration number,
    // mail-order filing, terms, privacy policy, main phone line.
    "사업자등록번호",
    "통신판매업",
    "사업자정보",
    "이용약관",
    "개인정보처리방침",
    "대표전화",
];

/// Sign-in prompts and fields, in the languages most pages use.
const AUTH: &[&str] = &[
    "sign in",
    "sign-in",
    "signin",
    "sign up",
    "sign-up",
    "signup",
    "log in",
    "log-in",
    "login",
    "log out",
    "logout",
    "register",
    "create account",
    "create an account",
    "create a free account",
    "create your account",
    "forgot password",
    "forgot your password",
    "reset password",
    "remember me",
    "signed in",
    "already have an account",
    "have an account",
    "continue with",
    "email or phone",
    "password",
    "guest mode",
    "not your computer",
    "join now",
    "join for free",
    "join free",
    "anmelden",
    "registrieren",
    "connexion",
    "se connecter",
    "s'inscrire",
    "iniciar sesión",
    "regístrate",
    "accedi",
    "registrati",
    "entrar",
    "cadastre-se",
    "войти",
    "регистрация",
    "登录",
    "登入",
    "注册",
    "密码",
    "验证码",
    "ログイン",
    "新規登録",
    "로그인",
    "회원가입",
];

/// Locale, currency and region prompts.
const LOCALE: &[&str] = &[
    "currency",
    "change language",
    "change the language",
    "select language",
    "select your language",
    "choose your language",
    "choose a language",
    "your region",
    "your country",
    "change region",
    "change country",
    "looks like you",
    "you are viewing",
    "view this site in",
    "switch to the",
];

/// Accessibility hints for widgets: instructions, not content.
const A11Y: &[&str] = &[
    "autocomplete results",
    "up and down arrows",
    "swipe gestures",
    "screen reader",
    "keyboard shortcuts",
];

/// App and newsletter promotion lines.
const PROMO: &[&str] = &[
    "newsletter",
    "subscribe to",
    "download the app",
    "get the app",
    "open in app",
    "open the app",
    "use the app",
    "install the app",
    "skip to",
];

/// Generic site-navigation words: a label made only of these is frame.
const NAV: &[&str] = &[
    "home",
    "menu",
    "search",
    "help",
    "about",
    "contact",
    "blog",
    "blogs",
    "news",
    "press",
    "newsroom",
    "careers",
    "jobs",
    "pricing",
    "plans",
    "features",
    "product",
    "products",
    "solutions",
    "enterprise",
    "resources",
    "docs",
    "documentation",
    "support",
    "community",
    "communities",
    "forum",
    "forums",
    "faq",
    "faqs",
    "download",
    "downloads",
    "apps",
    "app",
    "mobile",
    "desktop",
    "ios",
    "android",
    "windows",
    "macos",
    "mac",
    "linux",
    "settings",
    "account",
    "profile",
    "dashboard",
    "notifications",
    "messages",
    "inbox",
    "explore",
    "discover",
    "trending",
    "popular",
    "new",
    "latest",
    "shop",
    "store",
    "cart",
    "wishlist",
    "sell",
    "partners",
    "affiliates",
    "affiliate",
    "investors",
    "developers",
    "api",
    "status",
    "security",
    "legal",
    "privacy",
    "terms",
    "cookies",
    "accessibility",
    "sitemap",
    "advertise",
    "advertising",
    "language",
    "languages",
    "english",
    "more",
    "back",
    "next",
    "previous",
    "close",
    "share",
    "follow",
    "subscribe",
    "newsletter",
    "rss",
    "facebook",
    "twitter",
    "x",
    "instagram",
    "youtube",
    "linkedin",
    "tiktok",
    "pinterest",
    "reddit",
    "discord",
    "github",
    "telegram",
    "whatsapp",
    "weibo",
    "wechat",
    "line",
    "snapchat",
    "threads",
    "mastodon",
    "bluesky",
    "twitch",
    "plurk",
    "vk",
    "upgrade",
    "premium",
    "pro",
    "guidelines",
    "tutorials",
    "templates",
    "learn",
    "learning",
    "events",
    "webinars",
    "academy",
    "gallery",
    "magazine",
    "stories",
    "customers",
    "company",
    "team",
    "mission",
    "brand",
    "brands",
    "overview",
    "page",
    "library",
    "libraries",
    "collections",
    "rankings",
    "contests",
    "requests",
    "challenges",
    "categories",
    "topics",
    "tags",
    "games",
    "music",
    "videos",
    "video",
    "photos",
    "images",
    "movies",
    "tv",
    "watch",
    "live",
    "sports",
    "shopping",
    "marketplace",
    "deals",
    "offers",
    "gifts",
    "prints",
    "courses",
    "tools",
    "reports",
    "projects",
    "integrations",
    "changelog",
    "roadmap",
    "feedback",
    "contacts",
];

/// Function words that may join navigation words in one label
/// ("Terms of Use", "About Us").
const NAV_GLUE: &[&str] = &[
    "and", "&", "the", "our", "us", "for", "of", "all", "my", "your", "a", "to", "with",
];

const NAV_PHRASES: &[&str] = &[
    "about us",
    "contact us",
    "help center",
    "help centre",
    "get started",
    "free trial",
    "try free",
    "start free trial",
    "book a demo",
    "request demo",
    "request a demo",
    "request live demo",
    "learn more",
    "see more",
    "view all",
    "see all",
    "show more",
    "read more",
    "gift cards",
    "app store",
    "google play",
    "success stories",
    "case studies",
    "investor relations",
    "skip to content",
    "skip to main content",
    "skip navigation",
    "main menu",
    "what's new",
    "our story",
    "press room",
    "media kit",
    "for business",
    "for teams",
    "for developers",
    "see plans and pricing",
    "cookie settings",
    "legal info",
];

/// Language names as a language picker lists them.
const LANGS: &[&str] = &[
    "english",
    "deutsch",
    "español",
    "français",
    "italiano",
    "português",
    "nederlands",
    "polski",
    "русский",
    "日本語",
    "한국어",
    "中文",
    "简体中文",
    "繁體中文",
    "中文(简体)",
    "中文(繁體)",
    "العربية",
    "türkçe",
    "svenska",
    "dansk",
    "norsk",
    "suomi",
    "čeština",
    "magyar",
    "română",
    "ελληνικά",
    "עברית",
    "हिन्दी",
    "ไทย",
    "ภาษาไทย",
    "tiếng việt",
    "bahasa indonesia",
    "bahasa melayu",
    "melayu",
    "indonesia",
    "українська",
    "filipino",
    "català",
    "eesti",
    "latviešu",
    "lietuvių",
    "hrvatski",
    "slovenčina",
    "slovenščina",
    "български",
    "српски",
    "afrikaans",
    "azərbaycan",
    "bosanski",
    "cymraeg",
    "gaeilge",
    "galego",
    "isizulu",
    "íslenska",
    "kiswahili",
    "euskara",
    "norsk bokmål",
    "বাংলা",
    "فارسی",
    "اردو",
    "தமிழ்",
    "తెలుగు",
    "मराठी",
    "ગુજરાતી",
    "ಕನ್ನಡ",
    "മലയാളം",
];

/// Class and id tokens that mark a frame region on pages without
/// semantic elements (CSS-module names such as `FooterBar_root__x1`
/// split into `footer`, `bar`, `root`, `x1`).
const FRAME_CLASS: &[&str] = &[
    "footer",
    "navbar",
    "navigation",
    "breadcrumb",
    "breadcrumbs",
    "cookie",
    "cookies",
    "consent",
    "gdpr",
    "onetrust",
    "topnav",
    "sitenav",
    "globalnav",
    "megamenu",
    "skiplink",
    "skiplinks",
];

const FRAME_ROLES: &[&str] = &[
    "navigation",
    "banner",
    "contentinfo",
    "dialog",
    "alertdialog",
    "search",
    "menubar",
];

/// Element ids client-side frameworks mount into.
const MOUNT_IDS: &[&str] = &[
    "root",
    "app",
    "__next",
    "__nuxt",
    "___gatsby",
    "svelte",
    "q-app",
    "main-app",
    "application",
    "react-root",
    "app-root",
    "mount",
    "page-root",
    "react-app",
    "spa-root",
    "appmountpoint",
    "app-container",
];

const SKIP: &[&str] = &[
    "script", "style", "template", "svg", "noscript", "head", "iframe", "object", "canvas", "math",
    "input", "textarea", "select", "option", "meta", "link",
];

const BLOCK: &[&str] = &[
    "p",
    "div",
    "li",
    "ul",
    "ol",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "tr",
    "td",
    "th",
    "table",
    "pre",
    "blockquote",
    "section",
    "article",
    "main",
    "nav",
    "header",
    "footer",
    "aside",
    "dialog",
    "form",
    "dl",
    "dt",
    "dd",
    "figure",
    "figcaption",
    "br",
    "hr",
    "details",
    "summary",
    "address",
    "fieldset",
    "legend",
    "label",
    "button",
    "tbody",
    "thead",
    "center",
];

/// Scripts-required and unsupported-browser notices.
static SCRIPTS_REQUIRED: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)browser (isn.t|is not|not) (compatible|supported)|supported browsers?|incompatible browser|browser was unable|blocked by your (firewall|browser|ad)|(enable|turn on|activate|allow)\s+(\w+\s+){0,3}javascript|javascript\s+(is\s+)?(disabled|required|must|is off|not enabled|to run)|requires?\s+(\w+\s+){0,2}javascript|need\s+(\w+\s+){0,2}javascript|without\s+(\w+\s+){0,2}javascript|unable to load|browser (is )?(not supported|unsupported|out of date|outdated)|(update|upgrade) your browser|unsupported browser",
    )
    .expect("scripts-required pattern")
});

/// Error, region-block and challenge notices.
static NOTICE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\btry again\b|\b(resource|page|file) not found\b|something went wrong|an error (has )?occurred|error occurred|isn.t available|is not available|could not be (found|loaded)|couldn.t (be )?(found|load)|no longer available|\boops\b|checking your browser|verify you are (a )?human|verifying you are (a )?human|are you a robot|unusual traffic|access denied|press (&|and) hold|complete the security check|attention required|one more step|this process is automatic|redirect to your requested content",
    )
    .expect("notice pattern")
});

/// A headline that refuses the reader's browser: it addresses that browser
/// ("your browser isn't supported", "we no longer support this browser") or
/// tells the reader to change it ("Please change browsers to use Slack").
/// A page about which browsers are supported names them; it does not say so.
static REFUSAL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\b(your|this) browser(\s+version)?\s+(is\s+)?(not|no longer|isn.t)\s+(\w+\s+)?(supported|compatible)\b|\bno longer support(s|ing|ed)?\s+(this|your)\s+browser|^\W*(please\s+)?(change|switch|update|upgrade)\s+(your\s+)?browsers?\b|^\W*unsupported\s+browser\b",
    )
    .expect("refusal pattern")
});

/// A headline that is an age gate: the whole headline asks for the reader's
/// birth date or age ("Please tell us: When were you born?", "Are you of
/// legal drinking age?", "You must be 21 or older to enter this site"). It
/// must end with the ask, so a headline that goes on ("Are you over 50?
/// Here's what to know") heads an article.
static AGE_GATE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)(\bwhen\s+were\s+you\s+born|\b(enter|confirm|verify|provide|select)\s+your\s+(date\s+of\s+birth|birth\s?date|birthday|age)|\bwhat(\s+is|.s)\s+your\s+(date\s+of\s+birth|birth\s?date|age)|\bare\s+you\s+(of\s+(legal\s+)?((drinking|smoking)\s+)?age|(over|at\s+least)\s+\d{2}(\s+years(\s+old)?)?|\d{2}\+?(\s+years(\s+old)?)?(\s+or\s+(older|over|above))?)|\byou\s+(must|have\s+to|need\s+to)\s+be\s+(of\s+(legal\s+)?((drinking|smoking)\s+)?age|(over\s+|at\s+least\s+)?\d{2}\+?(\s+years(\s+old)?)?(\s+or\s+(older|over|above))?)|^\W*age\s+(verification|gate|check|confirmation))(\s+to\s+(enter|continue|proceed|visit|access|view)(\s+(this|the|our)\s+(site|website|page|store))?)?\W*$",
    )
    .expect("age gate pattern")
});

/// A headline that is a bot check or an access refusal standing in for the
/// page: the whole headline is the check ("Verifying your browser", "Just a
/// moment...", "Request Access", "Access denied", "Are you a robot?"),
/// optionally followed by where it applies ("before accessing example.com").
/// A headline that goes on ("Access denied: what a 403 means") heads an
/// article.
static CHECK: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)^\W*((please\s+)?(wait\s+while\s+we\s+)?(verifying|checking|verify|check)\s+(that\s+)?(your|the)\s+(browser|connection|device|request|access)|(verifying|verify|confirm)\s+(that\s+)?you\s+are\s+(a\s+)?human|are\s+you\s+a\s+(robot|human|bot)|just\s+a\s+moment|one\s+more\s+step|request(ing)?\s+access|you\s+need\s+(access|permission)|access\s+(denied|blocked|restricted|to\s+this\s+page\s+has\s+been\s+(denied|blocked))|(sorry,?\s+)?you\s+have\s+been\s+blocked|(security|bot|human|browser)\s+(check|verification|test))(\s+(before|to|for|on)\b.{0,80})?\W*$",
    )
    .expect("check pattern")
});

static LOADING: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)^(loading|please wait|one moment|just a moment|connecting|initiali[sz]ing|starting|fetching|preparing|finding|searching|redirecting|processing|waiting|checking)\b",
    )
    .expect("loading pattern")
});

static ING_WORD: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\b\w+ing\b").expect("ing pattern"));

// ------------------------------------------------------------------ walking

#[derive(Clone, Copy, Default)]
struct Ctx {
    frame: bool,
    main: bool,
    authform: bool,
    link: usize,
    mount: usize,
    heading: u8,
    /// The heading element the text is in: the lines of one heading split
    /// by `<br>` share it.
    heading_id: usize,
    pre: bool,
    row: usize,
    /// A header or footer kept out of the frame for holding most of the
    /// page's text (see `walk`).
    wrapper: usize,
}

/// One block of visible text and the structure around it.
struct Unit {
    text: String,
    /// The text split where one link ends and the next begins: a nav row
    /// of glued link labels is many labels, not one sentence.
    segments: Vec<String>,
    link_chars: usize,
    ctx: Ctx,
}

#[derive(Default)]
struct Builder {
    units: Vec<Unit>,
    text: String,
    segment: String,
    segments: Vec<String>,
    link_chars: usize,
    last_link: usize,
    ctx: Option<Ctx>,
}

impl Builder {
    fn push(&mut self, raw: &str, ctx: Ctx) {
        if raw.trim().is_empty() {
            if !self.text.is_empty() {
                self.text.push(' ');
                self.segment.push(' ');
            }
            return;
        }
        if self.ctx.is_none() {
            self.ctx = Some(ctx);
        }
        if ctx.link != self.last_link && !self.segment.trim().is_empty() {
            self.segments.push(collapse(&self.segment));
            self.segment.clear();
        }
        self.last_link = ctx.link;
        if ctx.link != 0 {
            self.link_chars += raw.chars().filter(|c| !c.is_whitespace()).count();
        }
        // Adjacent links with no space between them are separate labels.
        if !self.text.is_empty() && !self.text.ends_with(' ') && ctx.link != 0 {
            self.text.push(' ');
        }
        self.text.push_str(raw);
        self.segment.push_str(raw);
    }

    fn flush(&mut self) {
        let text = collapse(&self.text);
        if !self.segment.trim().is_empty() {
            self.segments.push(collapse(&self.segment));
        }
        if let Some(ctx) = self.ctx.take()
            && !text.is_empty()
        {
            self.units.push(Unit {
                text,
                segments: std::mem::take(&mut self.segments),
                link_chars: self.link_chars,
                ctx,
            });
        }
        self.text.clear();
        self.segment.clear();
        self.segments.clear();
        self.link_chars = 0;
        self.last_link = 0;
    }
}

fn collapse(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Class and id tokens: split on non-alphanumerics and camelCase seams.
fn tokens(attr: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut prev_lower = false;
    for c in attr.chars() {
        if !c.is_ascii_alphanumeric() {
            if !cur.is_empty() {
                out.push(std::mem::take(&mut cur).to_ascii_lowercase());
            }
            prev_lower = false;
            continue;
        }
        if c.is_ascii_uppercase() && prev_lower && !cur.is_empty() {
            out.push(std::mem::take(&mut cur).to_ascii_lowercase());
        }
        prev_lower = c.is_ascii_lowercase();
        cur.push(c);
    }
    if !cur.is_empty() {
        out.push(cur.to_ascii_lowercase());
    }
    out
}

/// Visible text characters under each header, footer and frame-classed
/// element, and under the body: one iterative pass, no recursion.
fn region_sizes(doc: &Html) -> (std::collections::HashMap<ego_tree::NodeId, usize>, usize) {
    let mut sizes = std::collections::HashMap::new();
    let root = doc.root_element();
    let body = root
        .children()
        .find(|c| c.value().as_element().is_some_and(|e| e.name() == "body"))
        .unwrap_or(*root);
    // (node, entered): on exit a node's total is folded into its parent's.
    let mut stack = vec![(body, false)];
    let mut sums: Vec<usize> = vec![0];
    while let Some((node, entered)) = stack.pop() {
        if entered {
            let total = sums.pop().unwrap_or(0);
            if let Some(el) = node.value().as_element() {
                let tracked = matches!(el.name(), "header" | "footer") || {
                    let attr = format!(
                        "{} {}",
                        el.attr("class").unwrap_or(""),
                        el.attr("id").unwrap_or("")
                    );
                    tokens(&attr)
                        .iter()
                        .any(|t| FRAME_CLASS.contains(&t.as_str()))
                };
                if tracked {
                    sizes.insert(node.id(), total);
                }
            }
            if let Some(parent) = sums.last_mut() {
                *parent += total;
            }
            continue;
        }
        match node.value() {
            Node::Text(t) => {
                if let Some(top) = sums.last_mut() {
                    *top += t.trim().chars().count();
                }
            }
            Node::Element(el) if !SKIP.contains(&el.name()) => {
                stack.push((node, true));
                sums.push(0);
                for child in node.children() {
                    stack.push((child, false));
                }
            }
            _ => {}
        }
    }
    let body_text = sums.pop().unwrap_or(0);
    (sizes, body_text)
}

struct Walk {
    units: Vec<Unit>,
    mounts: usize,
    noscript: Vec<String>,
    script_chars: usize,
    bundle_scripts: bool,
}

fn walk(doc: &Html) -> Walk {
    // Elements that wrap the main content: a region holding it is never
    // frame, whatever it is called (craigslist's `<article class=
    // "flexed-footer">`, a `<header>` around the whole page).
    let mut wraps_main = HashSet::new();
    let mut auth_forms = HashSet::new();
    for node in doc.tree.nodes() {
        let Some(el) = node.value().as_element() else {
            continue;
        };
        let name = el.name();
        let is_main = matches!(name, "main" | "article")
            || el
                .attr("role")
                .is_some_and(|r| r.eq_ignore_ascii_case("main"));
        if is_main {
            for anc in node.ancestors() {
                wraps_main.insert(anc.id());
            }
            wraps_main.insert(node.id());
        }
        if name == "input"
            && el.attr("type").is_some_and(|t| {
                t.eq_ignore_ascii_case("password") || t.eq_ignore_ascii_case("email")
            })
            && let Some(form) = node
                .ancestors()
                .find(|a| a.value().as_element().is_some_and(|e| e.name() == "form"))
        {
            auth_forms.insert(form.id());
        }
    }

    let (region_text, body_text) = region_sizes(doc);
    let mut out = Walk {
        units: Vec::new(),
        mounts: 0,
        noscript: Vec::new(),
        script_chars: 0,
        bundle_scripts: false,
    };
    let root = doc.root_element();
    let body = root
        .children()
        .find(|c| c.value().as_element().is_some_and(|e| e.name() == "body"))
        .unwrap_or(*root);

    enum Step<N> {
        Enter(N),
        Exit(bool),
    }
    let mut b = Builder::default();
    let mut ctxs = vec![Ctx::default()];
    let mut next_id = 0usize;
    let mut steps = vec![Step::Enter(body)];
    // Scripts and noscript anywhere in the document (head included).
    for node in doc.tree.nodes() {
        let Some(el) = node.value().as_element() else {
            continue;
        };
        match el.name() {
            "script" => {
                let ty = el.attr("type").unwrap_or("").to_ascii_lowercase();
                if ty.contains("json") {
                    continue;
                }
                if ty == "module" {
                    out.bundle_scripts = true;
                }
                if let Some(src) = el.attr("src") {
                    let src = src.to_ascii_lowercase();
                    if [
                        "/_next/",
                        "/_nuxt/",
                        "/static/js/",
                        "webpack",
                        "chunk",
                        "/assets/index-",
                    ]
                    .iter()
                    .any(|m| src.contains(m))
                    {
                        out.bundle_scripts = true;
                    }
                }
                out.script_chars += node
                    .children()
                    .filter_map(|c| c.value().as_text().map(|t| t.len()))
                    .sum::<usize>();
            }
            "noscript" => {
                let raw: String = node
                    .children()
                    .filter_map(|c| c.value().as_text().map(|t| t.to_string()))
                    .collect();
                if !raw.trim().is_empty() {
                    out.noscript.push(raw);
                }
            }
            _ => {}
        }
    }
    while let Some(step) = steps.pop() {
        match step {
            Step::Exit(block) => {
                ctxs.pop();
                if block {
                    b.flush();
                }
            }
            Step::Enter(node) => match node.value() {
                Node::Text(t) => {
                    let ctx = *ctxs.last().expect("context stack");
                    b.push(t, ctx);
                }
                Node::Element(el) => {
                    let name = el.name();
                    if SKIP.contains(&name) {
                        continue;
                    }
                    let parent = *ctxs.last().expect("context stack");
                    let mut ctx = parent;
                    next_id += 1;
                    let role = el.attr("role").unwrap_or("").to_ascii_lowercase();
                    let wraps = wraps_main.contains(&node.id());
                    let class_frame = {
                        let attr = format!(
                            "{} {}",
                            el.attr("class").unwrap_or(""),
                            el.attr("id").unwrap_or("")
                        );
                        tokens(&attr)
                            .iter()
                            .any(|t| FRAME_CLASS.contains(&t.as_str()))
                    };
                    // A header, footer or footer-classed block holding most
                    // of the page's text is a layout wrapper, not a banner
                    // (a Webflow article inside `<header class=
                    // "section-hero-header">`). Navigation and dialogs stay
                    // frame whatever they hold: a mega-menu is all labels.
                    let minority = || {
                        region_text
                            .get(&node.id())
                            .is_none_or(|n| n * 2 <= body_text)
                    };
                    let banner_like =
                        class_frame || (matches!(name, "header" | "footer") && !parent.main);
                    if !wraps
                        && (matches!(name, "nav" | "dialog")
                            || FRAME_ROLES.contains(&role.as_str())
                            || el.attr("aria-modal") == Some("true")
                            || (banner_like && minority()))
                    {
                        ctx.frame = true;
                    } else if !wraps && banner_like && ctx.wrapper == 0 {
                        // Judged after classification: a wrapper with no
                        // prose in it is frame after all.
                        ctx.wrapper = next_id;
                    }
                    if matches!(name, "main" | "article") || role == "main" {
                        ctx.main = true;
                    }
                    if name == "form" && auth_forms.contains(&node.id()) {
                        ctx.authform = true;
                    }
                    if matches!(name, "a" | "button")
                        || matches!(role.as_str(), "button" | "link" | "menuitem" | "tab")
                    {
                        ctx.link = next_id;
                    }
                    let id = el.attr("id").unwrap_or("").to_ascii_lowercase();
                    if ctx.mount == 0 && (MOUNT_IDS.contains(&id.as_str()) || name == "app-root") {
                        out.mounts += 1;
                        ctx.mount = next_id;
                    }
                    if let Some(level) = name
                        .strip_prefix('h')
                        .and_then(|d| d.parse::<u8>().ok())
                        .filter(|l| (1..=6).contains(l))
                    {
                        ctx.heading = level;
                        ctx.heading_id = next_id;
                    }
                    if name == "pre" {
                        ctx.pre = true;
                    }
                    if name == "tr" {
                        ctx.row = next_id;
                    }
                    let block = BLOCK.contains(&name);
                    if block {
                        b.flush();
                    }
                    ctxs.push(ctx);
                    steps.push(Step::Exit(block));
                    let children: Vec<_> = node.children().collect();
                    for child in children.into_iter().rev() {
                        steps.push(Step::Enter(child));
                    }
                }
                _ => {}
            },
        }
    }
    b.flush();
    out.units = b.units;
    out
}

// ---------------------------------------------------------- classification

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    Sentence,
    Data,
    Code,
    Entity,
    Frame,
    Auth,
    Loading,
    ScriptsRequired,
    Notice,
    Neutral,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Label {
    Empty,
    Loading,
    Lang,
    Legal,
    Auth,
    Locale,
    Promo,
    Nav,
    Entity,
}

fn has(lower: &str, list: &[&str]) -> bool {
    list.iter().any(|m| lower.contains(m))
}

fn words(text: &str) -> Vec<&str> {
    text.split(|c: char| !(c.is_alphanumeric() || c == '\'' || c == '’' || c == '-'))
        .filter(|w| w.chars().next().is_some_and(|c| c.is_alphabetic()))
        .collect()
}

fn is_cjk(c: char) -> bool {
    matches!(c as u32,
        0x3040..=0x30ff | 0x3400..=0x4dbf | 0x4e00..=0x9fff | 0xac00..=0xd7af | 0xf900..=0xfaff)
}

/// A sentence: words in running prose, not a label run.
fn is_sentence(text: &str) -> bool {
    let cjk = text.chars().filter(|c| is_cjk(*c)).count();
    if cjk >= 10 {
        return text.chars().any(|c| "，。！？、：；".contains(c)) || cjk >= 20;
    }
    let ws = words(text);
    if ws.is_empty() {
        return false;
    }
    let trimmed = text.trim_end();
    let ends = trimmed
        .trim_end_matches(['"', '”', '’', ')', '\''])
        .ends_with(['.', '!', '?', '…', '।', '۔', '؟']);
    let inner = text.char_indices().any(|(i, c)| {
        matches!(c, '.' | '!' | '?')
            && text[i + c.len_utf8()..].starts_with(' ')
            && text[i + c.len_utf8()..]
                .trim_start()
                .chars()
                .next()
                .is_some()
    });
    let term = ends || inner;
    let cased: Vec<&&str> = ws
        .iter()
        .filter(|w| {
            w.chars()
                .next()
                .is_some_and(|c| c.is_lowercase() || c.is_uppercase())
        })
        .collect();
    if cased.len() * 2 >= ws.len() {
        if ends && (trimmed.ends_with("...") || trimmed.ends_with('…')) && ws.len() <= 6 {
            return false;
        }
        let lower = cased
            .iter()
            .filter(|w| w.chars().next().is_some_and(char::is_lowercase))
            .count();
        return (ws.len() >= 6 || (term && ws.len() >= 4)) && lower * 10 >= cased.len() * 4;
    }
    ws.len() >= 10 || (term && ws.len() >= 6)
}

/// A token that states a quantity: 19K, 30%, $9.99, 8:50, 1.2M, 4230.
/// A bare year is not data; neither is an identifier that starts with a
/// digit (a hash, a hex id).
fn has_data(text: &str) -> bool {
    text.split_whitespace().any(|raw| {
        let t = raw
            .trim_start_matches([
                '$', '€', '£', '¥', '₹', '(', '[', '"', '\'', '+', '-', '~', '≈',
            ])
            .trim_end_matches([',', '.', ';', ':', ')', ']', '"', '\'', '!', '?', '*']);
        let mut chars = t.chars().peekable();
        if !chars.peek().is_some_and(|c| c.is_ascii_digit()) {
            return false;
        }
        let digits: String = t
            .chars()
            .take_while(|c| c.is_ascii_digit() || matches!(c, ',' | '.' | ':'))
            .collect();
        let rest = &t[digits.len()..];
        let unit_ok = rest.is_empty()
            || matches!(
                rest.to_lowercase().as_str(),
                "%" | "k"
                    | "m"
                    | "b"
                    | "bn"
                    | "°"
                    | "°c"
                    | "°f"
                    | "ms"
                    | "s"
                    | "mb"
                    | "gb"
                    | "kb"
                    | "tb"
                    | "x"
                    | "+"
                    | "h"
                    | "min"
                    | "km"
                    | "kg"
                    | "万"
                    | "亿"
                    | "千"
            );
        if !unit_ok {
            return false;
        }
        let year = rest.is_empty()
            && digits.len() == 4
            && (digits.starts_with("19") || digits.starts_with("20"));
        !year
    })
}

fn label_kind(text: &str) -> Label {
    let l = text
        .to_lowercase()
        .trim_matches(|c: char| c.is_whitespace() || ":|·•-–—>»›".contains(c))
        .to_string();
    if l.is_empty() {
        return Label::Empty;
    }
    let n_words = words(&l).len();
    if LOADING.is_match(&l) && n_words <= 8 {
        return Label::Loading;
    }
    let bare = l.split(" (").next().unwrap_or(&l).trim();
    if LANGS.contains(&l.as_str()) || LANGS.contains(&bare) && l.ends_with(')') {
        return Label::Lang;
    }
    if has(&l, LEGAL) {
        return Label::Legal;
    }
    if has(&l, AUTH) {
        return Label::Auth;
    }
    if has(&l, LOCALE) {
        return Label::Locale;
    }
    if has(&l, PROMO) {
        return Label::Promo;
    }
    if NAV.contains(&l.as_str()) || NAV_PHRASES.contains(&l.as_str()) {
        return Label::Nav;
    }
    let ws: Vec<String> = words(&l).into_iter().map(str::to_string).collect();
    if !ws.is_empty()
        && ws.len() <= 3
        && ws
            .iter()
            .all(|w| NAV.contains(&w.as_str()) || NAV_GLUE.contains(&w.as_str()))
    {
        return Label::Nav;
    }
    Label::Entity
}

fn classify(u: &Unit, auth_section: bool, echoes: &HashSet<String>) -> Kind {
    let t = u.text.as_str();
    let lower = t.to_lowercase();
    let n_words = words(t).len();
    let nonspace = t.chars().filter(|c| !c.is_whitespace()).count().max(1);
    let link_share = u.link_chars as f32 / nonspace as f32;
    if SCRIPTS_REQUIRED.is_match(t) {
        return Kind::ScriptsRequired;
    }
    if u.ctx.frame {
        return Kind::Frame;
    }
    if u.ctx.authform || (auth_section && u.ctx.heading == 0) {
        return Kind::Auth;
    }
    if u.ctx.pre && t.split_whitespace().count() >= 2 {
        return Kind::Code;
    }
    if NOTICE.is_match(t) {
        return Kind::Notice;
    }
    if !t.contains(' ')
        && t.chars().count() >= 16
        && t.chars().any(|c| c.is_ascii_digit())
        && t.chars().any(|c| c.is_ascii_alphabetic())
    {
        return Kind::Neutral;
    }
    if echoes.contains(lower.trim()) {
        return Kind::Neutral;
    }
    // Framework residue leaked as text (React server dumps:
    // `is_latency_sensitive_broadcast`, `FDSTooltipDef`, `[["low"]]`).
    let toks: Vec<&str> = t.split_whitespace().collect();
    if !toks.is_empty() && toks.iter().filter(|w| is_identifier(w)).count() * 2 >= toks.len() {
        return Kind::Neutral;
    }
    let trimmed = t.trim_end();
    if n_words <= 10
        && (LOADING.is_match(&lower)
            || ((trimmed.ends_with("...") || trimmed.ends_with('…')) && ING_WORD.is_match(t)))
    {
        return Kind::Loading;
    }
    if has(&lower, LEGAL)
        || has(&lower, LOCALE)
        || has(&lower, A11Y)
        || (has(&lower, PROMO) && n_words <= 12)
    {
        return Kind::Frame;
    }
    if is_sentence(t) && !(link_share >= 0.6 && u.segments.len() >= 3) {
        if has(&lower, AUTH) && (link_share >= 0.5 || n_words <= 8) {
            return Kind::Auth;
        }
        return Kind::Sentence;
    }
    let kinds: Vec<Label> = u.segments.iter().map(|s| label_kind(s)).collect();
    if kinds.contains(&Label::Loading) {
        return Kind::Loading;
    }
    let framed = |k: &Label| matches!(k, Label::Nav | Label::Lang | Label::Legal | Label::Auth);
    if has_data(t) && !kinds.iter().all(framed) {
        return Kind::Data;
    }
    if kinds.iter().all(|k| {
        matches!(
            k,
            Label::Nav
                | Label::Lang
                | Label::Legal
                | Label::Auth
                | Label::Locale
                | Label::Promo
                | Label::Empty
        )
    }) {
        return if kinds.contains(&Label::Auth) {
            Kind::Auth
        } else {
            Kind::Frame
        };
    }
    Kind::Entity
}

/// The page's declared alternate for an English reader: `x-default`,
/// then `en`, then any `en-*`, resolved against the page URL.
fn language_alternate(doc: &Html, base: Option<&url::Url>) -> Option<String> {
    let mut best: Option<(u8, String)> = None;
    for node in doc.tree.nodes() {
        let Some(el) = node.value().as_element() else {
            continue;
        };
        if el.name() != "link"
            || !el.attr("rel").is_some_and(|r| {
                r.split_whitespace()
                    .any(|t| t.eq_ignore_ascii_case("alternate"))
            })
        {
            continue;
        }
        let (Some(lang), Some(href)) = (el.attr("hreflang"), el.attr("href")) else {
            continue;
        };
        let lang = lang.to_ascii_lowercase();
        let rank = match lang.as_str() {
            "x-default" => 0,
            "en" => 1,
            l if l.starts_with("en-") => 2,
            _ => continue,
        };
        let href = match base {
            Some(b) => match b.join(href) {
                Ok(u) => u.to_string(),
                Err(_) => continue,
            },
            None => href.to_string(),
        };
        if base.is_some_and(|b| b.as_str() == href) {
            continue;
        }
        if best.as_ref().is_none_or(|(r, _)| rank < *r) {
            best = Some((rank, href));
        }
    }
    best.map(|(_, href)| href)
}

/// A token that names something to a program: snake_case, a long
/// camelCase name, a bracketed literal.
fn is_identifier(token: &str) -> bool {
    let t = token.trim_matches(|c: char| matches!(c, ',' | ';' | '.' | ':' | '"' | '\''));
    if t.starts_with(['[', '{']) {
        return true;
    }
    let chars: Vec<char> = t.chars().collect();
    let snake = chars
        .windows(3)
        .any(|w| w[0].is_alphanumeric() && w[1] == '_' && w[2].is_alphanumeric());
    let camel = chars.len() >= 10
        && chars.iter().all(|c| c.is_ascii_alphanumeric())
        && chars
            .windows(2)
            .any(|w| w[0].is_ascii_lowercase() && w[1].is_ascii_uppercase());
    snake || camel
}

/// Text and declared redirect of one `<noscript>` element's raw markup.
fn noscript_parts(raw: &str, base: Option<&url::Url>) -> (String, Option<String>) {
    let frag = Html::parse_fragment(raw);
    let mut redirect = None;
    let mut text = String::new();
    for node in frag.tree.nodes() {
        match node.value() {
            Node::Element(el) if el.name() == "meta" => {
                if el
                    .attr("http-equiv")
                    .is_some_and(|h| h.eq_ignore_ascii_case("refresh"))
                    && let Some(content) = el.attr("content")
                    && let Some(pos) = content.to_ascii_lowercase().find("url=")
                {
                    let target = content[pos + 4..].trim().trim_matches(['\'', '"']);
                    redirect = match base {
                        Some(b) => b.join(target).ok().map(|u| u.to_string()),
                        None => Some(target.to_string()),
                    };
                }
            }
            Node::Text(t) => {
                let inside_code = node.ancestors().any(|a| {
                    a.value()
                        .as_element()
                        .is_some_and(|e| matches!(e.name(), "style" | "script"))
                });
                if !inside_code {
                    text.push(' ');
                    text.push_str(t);
                }
            }
            _ => {}
        }
    }
    (collapse(&text), redirect)
}

/// Assess a parsed document. `url` resolves noscript redirects and names
/// the query values a page echoes back (a search page repeating its query
/// is not content).
pub fn assess(doc: &Html, url: &str) -> Page {
    let base = url::Url::parse(url).ok();
    let echoes: HashSet<String> = base
        .as_ref()
        .map(|u| {
            u.query_pairs()
                .map(|(_, v)| v.trim().to_lowercase())
                .filter(|v| !v.is_empty())
                .collect()
        })
        .unwrap_or_default();
    let w = walk(doc);

    let mut noscript_text = Vec::new();
    let mut noscript_redirect = None;
    let mut scripts_notice = false;
    for raw in &w.noscript {
        let (text, redirect) = noscript_parts(raw, base.as_ref());
        if redirect.is_some() && noscript_redirect.is_none() {
            noscript_redirect = redirect;
        }
        if text.is_empty() {
            continue;
        }
        let lower = text.to_lowercase();
        if lower.contains("javascript")
            || lower.contains("redirect")
            || lower.contains("browser")
            || lower
                .split(|c: char| !c.is_alphanumeric())
                .any(|t| t == "js")
            || NOTICE.is_match(&text)
        {
            scripts_notice = true;
        } else {
            noscript_text.push(text);
        }
    }

    let mut kinds = Vec::with_capacity(w.units.len());
    let mut auth_level: Option<u8> = None;
    // A page whose headline refuses the reader's browser, checks it for a
    // bot, refuses access, or asks the reader's age before letting them in,
    // is that notice: what stands
    // under the headline (an apology, a help link, a list of browsers to
    // install, the drink-responsibly line) is the notice too. Any later
    // heading ends it, so an article under such a headline keeps its
    // sections as content.
    let mut refusal = false;
    // The first heading element's id and text (its lines may be several
    // units), the units it spans, and whether a later heading has passed.
    let mut headline: Option<(usize, String)> = None;
    let mut headline_units: Vec<usize> = Vec::new();
    let mut past_headline = false;
    // Whether text other than headings has followed the headline: until it
    // does, a heading is the headline's subtitle ("Verifying your browser"
    // over "Complete the check below"), not a section of its own.
    let mut body_seen = false;
    let mut refusal_headline: Option<String> = None;
    let mut rows: Vec<(usize, usize)> = Vec::new();
    for u in &w.units {
        if u.ctx.heading > 0 && !u.ctx.frame && !past_headline {
            match &mut headline {
                Some((id, text)) if *id == u.ctx.heading_id => {
                    text.push(' ');
                    text.push_str(u.text.trim());
                }
                Some(_) if !body_seen => {}
                Some(_) => past_headline = true,
                None => headline = Some((u.ctx.heading_id, u.text.trim().to_string())),
            }
            refusal = !past_headline
                && headline.as_ref().is_some_and(|(_, t)| {
                    REFUSAL.is_match(t) || AGE_GATE.is_match(t) || CHECK.is_match(t)
                });
            if !past_headline {
                headline_units.push(kinds.len());
            }
            // The headline's earlier lines are the notice too, and the
            // whole headline is what the page says.
            if refusal {
                refusal_headline = headline.as_ref().map(|(_, t)| t.clone());
                for &i in &headline_units {
                    if matches!(
                        kinds.get(i),
                        Some(Kind::Sentence | Kind::Data | Kind::Entity)
                    ) {
                        kinds[i] = Kind::Notice;
                    }
                }
            }
        } else if u.ctx.heading == 0 && !u.ctx.frame && headline.is_some() {
            body_seen = true;
        }
        if u.ctx.heading > 0 {
            if auth_level.is_some_and(|l| u.ctx.heading <= l) {
                auth_level = None;
            }
            // Only a bare call to action opens a sign-in section ("Sign
            // Up", "Log in"); a title about signing in ("How to sign in to
            // your account") heads an article.
            if label_kind(&u.text) == Label::Auth && words(&u.text).len() <= 4 {
                auth_level = Some(u.ctx.heading);
            }
        }
        let mut kind = classify(u, auth_level.is_some(), &echoes);
        if refusal && matches!(kind, Kind::Sentence | Kind::Data | Kind::Entity) {
            kind = Kind::Notice;
        }
        if u.ctx.row != 0
            && !matches!(
                kind,
                Kind::Frame | Kind::Auth | Kind::Code | Kind::Notice | Kind::ScriptsRequired
            )
        {
            match rows.iter_mut().find(|(r, _)| *r == u.ctx.row) {
                Some((_, n)) => *n += 1,
                None => rows.push((u.ctx.row, 1)),
            }
        }
        kinds.push(kind);
    }

    // A page-wide header or footer without a sentence in it was a frame
    // region after all (a near-empty app page whose banner holds most of
    // its text: a name, a like count, a status).
    let prose_wrappers: HashSet<usize> = w
        .units
        .iter()
        .zip(&kinds)
        .filter(|(u, k)| u.ctx.wrapper != 0 && **k == Kind::Sentence)
        .map(|(u, _)| u.ctx.wrapper)
        .collect();
    for (u, k) in w.units.iter().zip(kinds.iter_mut()) {
        if u.ctx.wrapper != 0
            && !prose_wrappers.contains(&u.ctx.wrapper)
            && matches!(k, Kind::Data | Kind::Entity | Kind::Sentence)
        {
            *k = Kind::Frame;
        }
    }
    let count = |k: Kind| kinds.iter().filter(|x| **x == k).count();
    let sentences = count(Kind::Sentence);
    let data = count(Kind::Data);
    let code = count(Kind::Code);
    let entity = count(Kind::Entity);
    let frame = count(Kind::Frame);
    let auth = count(Kind::Auth);
    let loading = count(Kind::Loading);
    let notices = count(Kind::Notice) + count(Kind::ScriptsRequired);
    let table_rows = rows.iter().filter(|(_, n)| *n >= 2).count();

    // A listing of named things in the main region is content (a directory,
    // a gallery of titles), unless that region is a sign-in page.
    let in_main = |k: Kind| {
        w.units
            .iter()
            .zip(&kinds)
            .filter(|(u, x)| u.ctx.main && **x == k)
            .count()
    };
    let main_entities = if in_main(Kind::Auth) >= 2 {
        0
    } else {
        in_main(Kind::Entity)
    };
    let core = sentences + data + code + table_rows + noscript_text.len();
    let listing = if main_entities >= 3 { main_entities } else { 0 };
    let substance = core + listing;

    // A mount point with no content inside it: the app never rendered.
    let empty_mount = w.mounts > 0
        && !w.units.iter().zip(&kinds).any(|(u, k)| {
            u.ctx.mount != 0 && matches!(k, Kind::Sentence | Kind::Data | Kind::Code | Kind::Entity)
        });
    let text_chars: usize = w.units.iter().map(|u| u.text.len()).sum();
    let app = empty_mount
        || scripts_notice
        || count(Kind::ScriptsRequired) > 0
        || w.script_chars >= text_chars.max(1) * 5
        || (w.bundle_scripts && text_chars == 0);
    let authform = w.units.iter().any(|u| u.ctx.authform);

    let notice = refusal_headline
        .as_deref()
        .or_else(|| {
            w.units
                .iter()
                .zip(&kinds)
                .find(|(_, k)| matches!(k, Kind::Notice | Kind::ScriptsRequired))
                .map(|(u, _)| u.text.as_str())
        })
        .map(|text| text.chars().take(200).collect::<String>());

    let verdict = if w.units.is_empty() && noscript_text.is_empty() {
        Verdict::Empty
    } else if refusal_headline.is_some() && substance == 0 {
        // The headline says what the page is (a bot check, an access or
        // browser refusal, an age gate), whatever sign-in links its frame
        // carries.
        Verdict::Notice
    } else if (authform || auth >= 3) && auth >= 2 && core <= 1 && listing == 0 {
        Verdict::AuthGate
    } else if substance > 0 {
        Verdict::Content
    } else if auth >= 2 && auth >= loading {
        Verdict::AuthGate
    } else if loading > 0 {
        Verdict::Loading
    } else if notices > 0 {
        Verdict::Notice
    } else if (frame >= 3 && frame >= entity) || (app && entity < 3) {
        Verdict::Shell
    } else {
        Verdict::Content
    };
    let language_hop = if verdict == Verdict::Empty {
        language_alternate(doc, base.as_ref())
    } else {
        None
    };
    Page {
        verdict,
        substance,
        language_hop,
        notice: if matches!(verdict, Verdict::Notice | Verdict::Loading | Verdict::Shell) {
            notice
        } else {
            None
        },
        noscript_redirect,
        noscript_text,
    }
}

/// The verdict on text recovered outside the document's markup (a page's
/// embedded application state): each line is one unit of main-region text,
/// judged by the same classifier as the document. Content needs a sentence,
/// a figure, code, or a listing of three or more named things; text with
/// none of those is not content, whatever its length.
pub fn assess_lines<'a>(lines: impl IntoIterator<Item = &'a str>) -> Verdict {
    let echoes = HashSet::new();
    let units: Vec<Unit> = lines
        .into_iter()
        .filter_map(|line| {
            let trimmed = line.trim();
            let heading = trimmed.chars().take_while(|c| *c == '#').count() as u8;
            let text = trimmed
                .trim_start_matches(['#', '-', '*', '>', '|'])
                .trim_end_matches('|')
                .trim();
            let text = crate::extract::view::link_text(text);
            (!text.is_empty()).then(|| Unit {
                segments: vec![text.clone()],
                text,
                link_chars: 0,
                ctx: Ctx {
                    main: true,
                    heading,
                    ..Ctx::default()
                },
            })
        })
        .collect();
    if units.is_empty() {
        return Verdict::Empty;
    }
    let kinds: Vec<Kind> = units.iter().map(|u| classify(u, false, &echoes)).collect();
    let count = |k: Kind| kinds.iter().filter(|x| **x == k).count();
    let core = count(Kind::Sentence) + count(Kind::Data) + count(Kind::Code);
    let auth = count(Kind::Auth);
    let entity = if auth >= 2 { 0 } else { count(Kind::Entity) };
    let listing = if entity >= 3 { entity } else { 0 };
    if auth >= 2 && core <= 1 && listing == 0 {
        Verdict::AuthGate
    } else if core + listing > 0 {
        Verdict::Content
    } else if count(Kind::Loading) > 0 {
        Verdict::Loading
    } else if count(Kind::Notice) + count(Kind::ScriptsRequired) > 0 {
        Verdict::Notice
    } else {
        Verdict::Shell
    }
}

/// Path segments that name a sign-in endpoint (a segment's stem before
/// any extension: `login.php` is `login`).
const LOGIN_SEGMENTS: &[&str] = &[
    "login",
    "log-in",
    "signin",
    "sign-in",
    "sign_in",
    "logon",
    "servicelogin",
    "authwall",
    "sso",
    "auth",
    "oauth",
    "oauth2",
    "authorize",
    "openid-connect",
    "saml",
];

fn names_login(url: &url::Url) -> bool {
    url.path_segments().is_some_and(|mut segs| {
        segs.any(|seg| {
            let stem = seg.split('.').next().unwrap_or(seg).to_ascii_lowercase();
            LOGIN_SEGMENTS.contains(&stem.as_str())
        })
    })
}

/// The read ended on a sign-in endpoint the caller did not ask for: the
/// site redirected an anonymous client to log in, so whatever the final
/// page says, it is not the requested content. A caller who asked for a
/// login page gets it.
pub fn login_redirect(requested: &str, final_url: &str) -> bool {
    let (Ok(req), Ok(fin)) = (url::Url::parse(requested), url::Url::parse(final_url)) else {
        return false;
    };
    names_login(&fin) && !names_login(&req)
}

#[cfg(test)]
#[path = "substance_tests.rs"]
mod tests;
