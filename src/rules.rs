//! Per-domain rules: the operator's `[rules.url."<pattern>"]` table.
//!
//! A rule is keyed by a host pattern and can refuse a fetch with the
//! operator's own message, or pin the tier for that host. DonSeTch ships no
//! rules of its own: the table is empty unless the operator writes one.
//!
//! v1 keys match the host and the scheme, nothing else:
//!
//! - `example.com` : the host and every subdomain, http and https
//! - `.example.com` : that host only, http and https
//! - `https://example.com`, `https://.example.com` : the same, one scheme only
//!
//! Every port, path and query matches. The single most specific matching
//! rule applies in full (winner-takes-all); see [`RuleSet::eval`].
//!
//! Where the checks run across the crate, the error contract and the
//! decisions behind them: `docs/rules-architecture.md`. Operator guide:
//! `docs/rules.md`.

// Keep docs/rules-architecture.md in step with this feature: a change to
// the matching, the error contract, where a check runs (here, the guards,
// the fetch, crawl and screenshot tools) or a decision it records needs the
// same change there, and docs/rules.md too when operators can see it.

use std::cmp::Reverse;
use std::collections::{BTreeMap, HashMap};
use std::sync::OnceLock;

use url::{Host, Url};

use crate::error::FetchError;

// ---------------------------------------------------------------------------
// Config types (deserialized from [rules] in donsetch.toml)
// ---------------------------------------------------------------------------

/// The switch on the whole mechanism. `Off` runs as if the table were empty,
/// tier pins included.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum RulesMode {
    #[default]
    Enforce,
    Off,
}

impl RulesMode {
    /// The config spelling: `"enforce"` or `"off"`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Enforce => "enforce",
            Self::Off => "off",
        }
    }
}

/// The `[rules]` section. Scalars directly under it apply to every rule kind;
/// `url` holds the `[rules.url."<pattern>"]` table.
#[derive(Debug, Clone, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct RulesSection {
    pub mode: RulesMode,
    /// URLs listed per rule in a crawl's denied section; 0 lists none.
    pub crawl_denied_urls_per_rule: u32,
    pub url: BTreeMap<String, UrlRule>,
}

impl Default for RulesSection {
    fn default() -> Self {
        Self {
            mode: RulesMode::Enforce,
            crawl_denied_urls_per_rule: 20,
            url: BTreeMap::new(),
        }
    }
}

/// What a winning rule does with the URL.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum RuleAction {
    #[default]
    Allow,
    Deny,
}

/// How a denial is classed for the agent and the CLI exit code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum DenyKind {
    /// Unavailable here; the content may be found elsewhere.
    #[default]
    Walled,
    /// Do not pursue the content at all.
    Permanent,
}

const WALLED_PHRASE: &str = "walled: try another source";
const PERMANENT_PHRASE: &str = "permanent: do not pursue";

impl DenyKind {
    /// The config and `errorKind` spelling: `"walled"` or `"permanent"`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Walled => "walled",
            Self::Permanent => "permanent",
        }
    }

    /// The short phrase shown in a denial's text line.
    pub fn phrase(self) -> &'static str {
        match self {
            Self::Walled => WALLED_PHRASE,
            Self::Permanent => PERMANENT_PHRASE,
        }
    }

    fn from_kind_str(kind: &str) -> Self {
        if kind == Self::Permanent.as_str() {
            Self::Permanent
        } else {
            Self::Walled
        }
    }
}

/// [`DenyKind::phrase`] keyed by the kind's string; anything other than
/// `"permanent"` reads as walled.
pub fn kind_phrase(kind: &str) -> &'static str {
    DenyKind::from_kind_str(kind).phrase()
}

/// The tier a rule asks for, spelled like the `tier` tool argument.
///
/// `Auto` with `tier_enforce` makes the host ignore a per-call tier.
/// Without `tier_enforce` it changes nothing, and is accepted so that a
/// higher config layer can cancel a lower layer's pin.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub enum RuleTier {
    #[serde(rename = "auto")]
    Auto,
    #[serde(rename = "1")]
    One,
    #[serde(rename = "2")]
    Two,
}

impl RuleTier {
    /// The tool-argument spelling: `"auto"`, `"1"` or `"2"`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::One => "1",
            Self::Two => "2",
        }
    }
}

/// One `[rules.url."<pattern>"]` entry, after layer merging.
///
/// Fields that do not apply to `action` are ignored, not rejected:
/// `message`, `kind` and `reason` matter only for `deny`.
#[derive(Debug, Clone, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct UrlRule {
    /// `false` removes the rule from matching, as if it were not written.
    pub enabled: bool,
    pub action: RuleAction,
    /// The operator's guidance for a denied URL, emitted verbatim.
    /// Required, and non-blank, when `action` is `deny`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    pub kind: DenyKind,
    /// Subcode of the denial's error code, `[a-z0-9_]+`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tier: Option<RuleTier>,
    /// `true` makes `tier` win over an explicit per-call `tier` argument.
    pub tier_enforce: bool,
}

impl Default for UrlRule {
    fn default() -> Self {
        Self {
            enabled: true,
            action: RuleAction::Allow,
            message: None,
            kind: DenyKind::Walled,
            reason: None,
            tier: None,
            tier_enforce: false,
        }
    }
}

// ---------------------------------------------------------------------------
// The denial payload
// ---------------------------------------------------------------------------

/// Everything a rule denial carries, from the guard to every surface.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Denial {
    /// The rule's key as written, e.g. `"banned.example"`.
    pub rule: String,
    /// The operator's message, verbatim and non-blank.
    pub message: String,
    /// `"walled"` or `"permanent"`.
    pub kind: String,
    /// The subcode, `[a-z0-9_]+`, when the rule sets one.
    pub reason: Option<String>,
}

impl Denial {
    /// The error code: `policy.denied.<reason>`, or
    /// `policy.denied.unspecified` without a reason. Always three segments.
    pub fn code(&self) -> String {
        policy_code(self.reason.as_deref())
    }

    /// [`Denial::kind`] as a static string; anything other than
    /// `"permanent"` reads as `"walled"`.
    pub fn kind_static(&self) -> &'static str {
        DenyKind::from_kind_str(&self.kind).as_str()
    }

    /// The [`FetchError::Denied`] carrying this payload.
    pub fn into_error(self) -> FetchError {
        let kind = self.kind_static();
        FetchError::Denied {
            rule: self.rule,
            message: self.message,
            kind,
            reason: self.reason,
        }
    }

    /// The payload of a [`FetchError::Denied`]; `None` for any other error.
    pub fn from_error(e: &FetchError) -> Option<Denial> {
        match e {
            FetchError::Denied {
                rule,
                message,
                kind,
                reason,
            } => Some(Denial {
                rule: rule.clone(),
                message: message.clone(),
                kind: (*kind).to_string(),
                reason: reason.clone(),
            }),
            _ => None,
        }
    }
}

/// The policy error code for a reason: `policy.denied.<reason>`, or
/// `policy.denied.unspecified` for `None`.
pub fn policy_code(reason: Option<&str>) -> String {
    format!("policy.denied.{}", reason.unwrap_or("unspecified"))
}

// ---------------------------------------------------------------------------
// The compiled matcher
// ---------------------------------------------------------------------------

/// One rule after its key compiled.
#[derive(Debug, Clone, PartialEq)]
pub struct CompiledRule {
    /// The key as written in the config.
    pub key: String,
    /// The normalized host, without the leading `.` of an exact key. An IPv6
    /// literal keeps its brackets.
    pub host: String,
    /// The key had a leading `.`: this host only, no subdomains.
    pub exact: bool,
    /// `Some("http")` or `Some("https")` for the scheme form; `None` matches
    /// both.
    pub scheme: Option<String>,
    pub rule: UrlRule,
}

impl CompiledRule {
    /// The denial this rule produces; `Some` exactly when `action` is `deny`.
    pub fn denial(&self) -> Option<Denial> {
        if self.rule.action != RuleAction::Deny {
            return None;
        }
        Some(Denial {
            rule: self.key.clone(),
            message: self.rule.message.clone().unwrap_or_default(),
            kind: self.rule.kind.as_str().to_string(),
            reason: self.rule.reason.clone(),
        })
    }

    fn labels(&self) -> usize {
        self.host.split('.').count()
    }
}

/// A compiled `[rules.url]` table, indexed by host.
#[derive(Debug, Default)]
pub struct RuleSet {
    rules: Vec<CompiledRule>,
    /// Exact-host keys (`.example.com`), by host.
    exact: HashMap<String, Vec<usize>>,
    /// Tree keys (`example.com`), by host; looked up once per suffix.
    tree: HashMap<String, Vec<usize>>,
}

impl RuleSet {
    /// The set with no rules: nothing matches.
    pub fn empty() -> RuleSet {
        RuleSet::default()
    }

    /// Compile and validate every key of `section.url`, whatever `mode` says.
    ///
    /// A key must already be in normal form (lowercase ASCII host, punycode
    /// for an IDN, no trailing dot, canonical IP literal) and carry no port,
    /// path, query, fragment, userinfo or `*`; `glob:` and `re:` are
    /// reserved. Two keys that compile to the same pattern are an error,
    /// disabled rules included. A `deny` rule needs a non-blank `message`
    /// and a `reason`, when set, matching `[a-z0-9_]+`.
    ///
    /// The error is the user-facing message, naming the key and what to
    /// write instead.
    pub fn compile(section: &RulesSection) -> Result<RuleSet, String> {
        let mut rules = Vec::with_capacity(section.url.len());
        for (key, rule) in &section.url {
            let parsed = parse_key(key)?;
            check_fields(key, rule)?;
            rules.push(CompiledRule {
                key: key.clone(),
                host: parsed.host,
                exact: parsed.exact,
                scheme: parsed.scheme,
                rule: rule.clone(),
            });
        }
        check_duplicates(&rules)?;
        Ok(RuleSet::from_compiled(rules))
    }

    fn from_compiled(rules: Vec<CompiledRule>) -> RuleSet {
        let mut exact: HashMap<String, Vec<usize>> = HashMap::new();
        let mut tree: HashMap<String, Vec<usize>> = HashMap::new();
        for (i, r) in rules.iter().enumerate() {
            let index = if r.exact { &mut exact } else { &mut tree };
            index.entry(r.host.clone()).or_default().push(i);
        }
        RuleSet { rules, exact, tree }
    }

    /// The number of rules, disabled ones included.
    pub fn len(&self) -> usize {
        self.rules.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    /// The winning enabled rule for `url`, or `None`.
    ///
    /// Candidates rank by host labels (more first), then exact over tree at
    /// the same host, then one scheme over both, then by key. The URL's host
    /// is compared with every trailing dot removed.
    pub fn eval(&self, url: &Url) -> Option<&CompiledRule> {
        self.matches(url).into_iter().find(|r| r.rule.enabled)
    }

    /// Every rule whose pattern matches `url`, enabled or not, most specific
    /// first: the order [`RuleSet::eval`] picks from.
    pub fn matches(&self, url: &Url) -> Vec<&CompiledRule> {
        if self.rules.is_empty() {
            return Vec::new();
        }
        let mut hits: Vec<usize> = Vec::new();
        match url.host() {
            None => return Vec::new(),
            Some(Host::Domain(domain)) => {
                // `Url` keeps a trailing dot, and `example.com.` reaches the
                // same site: strip them all so it cannot evade a deny.
                let domain = domain.trim_end_matches('.');
                if domain.is_empty() {
                    return Vec::new();
                }
                if let Some(ids) = self.exact.get(domain) {
                    hits.extend(ids);
                }
                let mut suffix = domain;
                loop {
                    if let Some(ids) = self.tree.get(suffix) {
                        hits.extend(ids);
                    }
                    match suffix.find('.') {
                        Some(i) => suffix = &suffix[i + 1..],
                        None => break,
                    }
                }
            }
            // An address has no subdomains: both forms match it alone.
            Some(ip) => {
                let host = ip.to_string();
                for index in [&self.exact, &self.tree] {
                    if let Some(ids) = index.get(&host) {
                        hits.extend(ids);
                    }
                }
            }
        }
        let scheme = url.scheme();
        let mut out: Vec<&CompiledRule> = hits
            .into_iter()
            .map(|i| &self.rules[i])
            .filter(|r| r.scheme.as_deref().is_none_or(|s| s == scheme))
            .collect();
        out.sort_by(|a, b| precedence(a).cmp(&precedence(b)));
        out
    }

    /// The denial for `url` when the winning rule is a `deny`.
    pub fn denial(&self, url: &Url) -> Option<Denial> {
        self.eval(url).and_then(CompiledRule::denial)
    }

    /// [`RuleSet::denial`] on a URL string; `None` when it does not parse.
    pub fn denial_for_str(&self, url: &str) -> Option<Denial> {
        if self.rules.is_empty() {
            return None;
        }
        Url::parse(url).ok().and_then(|u| self.denial(&u))
    }
}

fn precedence(r: &CompiledRule) -> (Reverse<usize>, Reverse<bool>, Reverse<bool>, &str) {
    (
        Reverse(r.labels()),
        Reverse(r.exact),
        Reverse(r.scheme.is_some()),
        r.key.as_str(),
    )
}

// ---------------------------------------------------------------------------
// The process-global ruleset
// ---------------------------------------------------------------------------

static RULESET: OnceLock<RuleSet> = OnceLock::new();

/// The process ruleset, compiled on first use from the installed config's
/// `[rules]`; empty when `mode` is `off`. Never fails.
pub fn rules() -> &'static RuleSet {
    RULESET.get_or_init(|| {
        let section = &crate::config::cfg().rules;
        if section.mode == RulesMode::Off {
            return RuleSet::empty();
        }
        // `config::validate()` compiled this same section before the config
        // was installed, so this error is unreachable from a real load.
        RuleSet::compile(section).unwrap_or_else(|e| {
            eprintln!("[donsetch rules] {e}; running with no rules");
            RuleSet::empty()
        })
    })
}

// ---------------------------------------------------------------------------
// Key parsing and validation
// ---------------------------------------------------------------------------

struct ParsedKey {
    host: String,
    exact: bool,
    scheme: Option<String>,
}

fn key_ref(key: &str) -> String {
    format!("[rules.url.{key:?}]")
}

fn key_error(key: &str, what: impl std::fmt::Display) -> String {
    format!("config error in {}: {what}", key_ref(key))
}

const NAME_A_HOST: &str = "Name the host the rule is for, e.g. \"example.com\".";

fn parse_key(key: &str) -> Result<ParsedKey, String> {
    let err = |what: String| Err(key_error(key, what));

    if key.is_empty() {
        return err(format!("an empty pattern is not supported.\n{NAME_A_HOST}"));
    }
    for prefix in ["glob:", "re:"] {
        if key.starts_with(prefix) {
            return err(format!(
                "\"{prefix}\" patterns are reserved and not yet supported.\n\
                 Write a host, e.g. \"example.com\": rules match the host and the scheme only."
            ));
        }
    }

    // A "://" after a path, query or fragment character is not a scheme.
    let (scheme, rest) = match key.find("://") {
        Some(i) if !key[..i].contains(['/', '?', '#']) => (Some(&key[..i]), &key[i + 3..]),
        _ => (None, key),
    };
    let (authority, tail) = match rest.find(['/', '?', '#']) {
        Some(i) => rest.split_at(i),
        None => (rest, ""),
    };
    let prefix = scheme.map(|s| format!("{s}://")).unwrap_or_default();

    // Wildcards first: they are the most likely habit from other formats.
    let dotless = authority.strip_prefix('.').unwrap_or(authority);
    let host_only = if dotless.starts_with('[') {
        dotless
    } else {
        dotless.split(':').next().unwrap_or(dotless)
    };
    if host_only == "*" {
        return err("a rule for every host is not supported.\n\
             Name the hosts the rule is for, e.g. \"example.com\"."
            .to_string());
    }
    if let Some(base) = host_only.strip_prefix("*.") {
        return err(format!(
            "\"*.\" is not supported in rule patterns.\n\
             \"{prefix}{base}\" already covers {base} and all its subdomains; \
             write \"{prefix}.{base}\" for that host only."
        ));
    }
    match scheme {
        None | Some("http") | Some("https") => {}
        Some("*") => {
            return err(format!(
                "\"*://\" is not supported in rule patterns.\n\
                 A pattern without a scheme already covers http and https : write \"{authority}\"."
            ));
        }
        Some(s) if s.eq_ignore_ascii_case("http") || s.eq_ignore_ascii_case("https") => {
            let lower = s.to_ascii_lowercase();
            return err(format!(
                "schemes must be written in lowercase in rule patterns.\n\
                 Write \"{lower}://{authority}\"."
            ));
        }
        Some(_) => {
            return err(format!(
                "only \"http\" and \"https\" schemes are supported in rule patterns.\n\
                 Write \"https://{authority}\" for one scheme, or \"{authority}\" for both."
            ));
        }
    }

    if let Some(c) = tail.chars().next() {
        let (what, every) = match c {
            '/' => ("paths are not supported in v1 rule patterns", "path"),
            '?' => ("queries are not supported in rule patterns", "query"),
            _ => ("fragments are not supported in rule patterns", "fragment"),
        };
        return err(format!(
            "{what}.\nA pattern matches every {every} : write \"{prefix}{authority}\"."
        ));
    }
    if let Some(i) = authority.rfind('@') {
        let host = &authority[i + 1..];
        return err(format!(
            "userinfo is not supported in rule patterns.\nWrite \"{prefix}{host}\"."
        ));
    }

    // Split off a port. Colons inside IPv6 brackets are not one.
    let (host, port) = if dotless.starts_with('[') {
        match authority.find(']') {
            Some(i) => authority.split_at(i + 1),
            None => (authority, ""),
        }
    } else {
        match authority.find(':') {
            Some(i) if authority[i + 1..].contains(':') => {
                let bare = authority.strip_prefix('.').unwrap_or(authority);
                return match bare.parse::<std::net::Ipv6Addr>() {
                    Ok(_) => {
                        let canonical = Host::parse(&format!("[{bare}]"))
                            .map(|h| h.to_string())
                            .unwrap_or_else(|_| format!("[{bare}]"));
                        err(format!(
                            "IPv6 literals must be in brackets in rule patterns.\n\
                             Write \"{prefix}{canonical}\"."
                        ))
                    }
                    Err(_) => err(format!(
                        "\"{authority}\" is not a valid host.\n{NAME_A_HOST}"
                    )),
                };
            }
            Some(i) => authority.split_at(i),
            None => (authority, ""),
        }
    };
    if !port.is_empty() {
        if port.starts_with(':') {
            return err(format!(
                "ports are not supported in rule patterns.\n\
                 A pattern matches every port : write \"{prefix}{host}\"."
            ));
        }
        return err(format!(
            "\"{authority}\" is not a valid host.\n{NAME_A_HOST}"
        ));
    }

    let exact = host.starts_with('.');
    let dot = if exact { "." } else { "" };
    let bare = &host[dot.len()..];
    if bare.is_empty() {
        return err(format!("the pattern names no host.\n{NAME_A_HOST}"));
    }
    if bare.starts_with('.') {
        let base = bare.trim_start_matches('.');
        return err(format!(
            "a pattern takes at most one leading \".\".\n\
             Write \"{prefix}.{base}\" for that host only, or \"{prefix}{base}\" for it and its subdomains."
        ));
    }
    if bare.contains('*') {
        return err("\"*\" is not supported in rule patterns.\n\
             Write the host itself, e.g. \"example.com\": it already covers all its subdomains."
            .to_string());
    }
    // `Host::parse` keeps a trailing dot, so it needs its own check.
    if bare.ends_with('.') {
        let trimmed = bare.trim_end_matches('.');
        return err(format!(
            "a trailing dot is not supported in rule patterns.\n\
             A pattern matches the host with or without one : write \"{prefix}{dot}{trimmed}\"."
        ));
    }
    if !bare.starts_with('[') && bare.split('.').any(str::is_empty) {
        let joined: Vec<&str> = bare.split('.').filter(|l| !l.is_empty()).collect();
        let joined = joined.join(".");
        return err(format!(
            "empty labels (\"..\") are not supported in rule patterns.\n\
             Write \"{prefix}{dot}{joined}\"."
        ));
    }

    let parsed = match Host::parse(bare) {
        Ok(h) => h,
        Err(e) => {
            return err(format!(
                "\"{bare}\" is not a valid host ({e}).\n{NAME_A_HOST}"
            ));
        }
    };
    let canonical = parsed.to_string();
    let is_ip = !matches!(parsed, Host::Domain(_));
    if is_ip && exact {
        return err(format!(
            "IP literals take no \".\" prefix: an address has no subdomains.\n\
             Write \"{prefix}{canonical}\"."
        ));
    }
    if canonical != bare {
        let suggestion = format!("{prefix}{dot}{canonical}");
        let what = if is_ip {
            "IP literals must be in canonical form in rule patterns.\nWrite".to_string()
        } else if !bare.is_ascii() {
            "non-ASCII hosts are not supported in rule patterns.\nWrite the punycode form:"
                .to_string()
        } else if bare.to_ascii_lowercase() == canonical {
            "hosts must be written in lowercase in rule patterns.\nWrite".to_string()
        } else {
            "the host is not in normal form.\nWrite".to_string()
        };
        return err(format!("{what} \"{suggestion}\"."));
    }

    Ok(ParsedKey {
        host: canonical,
        exact,
        scheme: scheme.map(str::to_string),
    })
}

/// Checks the fields that apply to the rule's merged `action`; the others are
/// ignored.
fn check_fields(key: &str, rule: &UrlRule) -> Result<(), String> {
    // Rejecting an inapplicable field would make an override a load error:
    // a higher layer turning a `deny` into an `allow` still carries the lower
    // layer's `message` and `reason`, and has no way to clear them.
    if rule.action != RuleAction::Deny {
        return Ok(());
    }
    if rule.message.as_deref().is_none_or(|m| m.trim().is_empty()) {
        return Err(key_error(
            key,
            "a \"deny\" rule needs a non-blank message: it is the guidance the agent \
             receives in place of the page.\n\
             Add e.g. message = \"ask the human operator to download the file\".",
        ));
    }
    if let Some(reason) = &rule.reason {
        let valid = !reason.is_empty()
            && reason
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_');
        if !valid {
            let mut fixed: String = reason
                .chars()
                .map(|c| {
                    if c.is_ascii_alphanumeric() {
                        c.to_ascii_lowercase()
                    } else {
                        '_'
                    }
                })
                .collect();
            if fixed.trim_matches('_').is_empty() {
                fixed = "ip_ban".to_string();
            }
            return Err(key_error(
                key,
                format!(
                    "reason {reason:?} may contain only lowercase letters, digits and \"_\".\n\
                     Write e.g. reason = \"{fixed}\"; it yields the code \"policy.denied.{fixed}\"."
                ),
            ));
        }
    }
    Ok(())
}

/// Rejects two keys that compile to the same pattern, disabled rules
/// included.
fn check_duplicates(rules: &[CompiledRule]) -> Result<(), String> {
    // Such keys would rank equally and be split only by the key-string
    // tie-break, so an override written in another spelling silently becomes
    // a second rule. A tombstone in another spelling disables nothing, which
    // is why disabled rules count.
    let mut seen: HashMap<(&str, bool, Option<&str>), &str> = HashMap::new();
    for r in rules {
        let pattern = (r.host.as_str(), r.exact, r.scheme.as_deref());
        if let Some(first) = seen.insert(pattern, r.key.as_str()) {
            return Err(format!(
                "config error: {} and {} are the same pattern.\n\
                 Keep one. To override a rule from another layer, use its exact key.",
                key_ref(first),
                key_ref(&r.key)
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn deny(message: &str) -> UrlRule {
        UrlRule {
            action: RuleAction::Deny,
            message: Some(message.to_string()),
            ..UrlRule::default()
        }
    }

    fn allow() -> UrlRule {
        UrlRule::default()
    }

    fn section(entries: Vec<(&str, UrlRule)>) -> RulesSection {
        RulesSection {
            url: entries
                .into_iter()
                .map(|(k, r)| (k.to_string(), r))
                .collect(),
            ..RulesSection::default()
        }
    }

    fn compile(entries: Vec<(&str, UrlRule)>) -> RuleSet {
        RuleSet::compile(&section(entries)).expect("ruleset compiles")
    }

    fn key_err(key: &str) -> String {
        match RuleSet::compile(&section(vec![(key, deny("go elsewhere"))])) {
            Ok(_) => panic!("{key:?} should be a load error"),
            Err(e) => e,
        }
    }

    fn url(s: &str) -> Url {
        Url::parse(s).expect("test url parses")
    }

    fn winner<'a>(set: &'a RuleSet, u: &str) -> Option<&'a str> {
        set.eval(&url(u)).map(|r| r.key.as_str())
    }

    // --- load errors -------------------------------------------------------

    #[test]
    fn port_is_a_load_error_naming_the_fix() {
        assert_eq!(
            key_err("example.com:8080"),
            "config error in [rules.url.\"example.com:8080\"]: ports are not supported in rule patterns.\n\
             A pattern matches every port : write \"example.com\"."
        );
        assert!(key_err("https://example.com:443").ends_with("write \"https://example.com\"."));
        assert!(key_err("[2001:db8::1]:8080").ends_with("write \"[2001:db8::1]\"."));
    }

    #[test]
    fn path_query_fragment_are_load_errors() {
        let e = key_err("https://example.com/");
        assert!(e.contains("paths are not supported"), "{e}");
        assert!(e.ends_with("write \"https://example.com\"."), "{e}");
        let e = key_err("example.com/docs");
        assert!(e.contains("paths are not supported"), "{e}");
        assert!(e.ends_with("write \"example.com\"."), "{e}");
        let e = key_err("example.com?a=1");
        assert!(e.contains("queries are not supported"), "{e}");
        assert!(e.ends_with("write \"example.com\"."), "{e}");
        let e = key_err("example.com#top");
        assert!(e.contains("fragments are not supported"), "{e}");
    }

    #[test]
    fn star_subdomain_is_a_load_error_with_both_spellings() {
        assert_eq!(
            key_err("*.example.com"),
            "config error in [rules.url.\"*.example.com\"]: \"*.\" is not supported in rule patterns.\n\
             \"example.com\" already covers example.com and all its subdomains; \
             write \".example.com\" for that host only."
        );
        let e = key_err("https://*.example.com");
        assert!(e.contains("\"https://example.com\" already covers"), "{e}");
        assert!(e.contains("write \"https://.example.com\""), "{e}");
    }

    #[test]
    fn catch_all_hosts_are_load_errors() {
        for key in ["*", "http://*", "https://*", "*://*", ".*", "*:8080"] {
            let e = key_err(key);
            assert!(
                e.contains("a rule for every host is not supported."),
                "{key}: {e}"
            );
        }
        assert_eq!(
            key_err("https://*"),
            "config error in [rules.url.\"https://*\"]: a rule for every host is not supported.\n\
             Name the hosts the rule is for, e.g. \"example.com\"."
        );
    }

    #[test]
    fn star_scheme_suggests_the_bare_host() {
        let e = key_err("*://example.com");
        assert!(e.contains("\"*://\" is not supported"), "{e}");
        assert!(e.ends_with("write \"example.com\"."), "{e}");
    }

    #[test]
    fn other_stars_and_schemes_are_load_errors() {
        assert!(key_err("exa*mple.com").contains("\"*\" is not supported"));
        assert!(key_err("ftp://example.com").contains("only \"http\" and \"https\""));
        let e = key_err("HTTPS://example.com");
        assert!(e.contains("lowercase"), "{e}");
        assert!(e.ends_with("Write \"https://example.com\"."), "{e}");
    }

    #[test]
    fn reserved_prefixes_are_load_errors() {
        assert!(key_err("glob:http*://*rezka*/*").contains("\"glob:\" patterns are reserved"));
        assert!(
            key_err("re:^https://(www\\.)?example\\.(com|org)/docs/")
                .contains("\"re:\" patterns are reserved")
        );
    }

    #[test]
    fn non_normal_hosts_are_load_errors_with_the_normal_spelling() {
        assert_eq!(
            key_err("bücher.de"),
            "config error in [rules.url.\"bücher.de\"]: non-ASCII hosts are not supported in rule patterns.\n\
             Write the punycode form: \"xn--bcher-kva.de\"."
        );
        let e = key_err("Banned.EXAMPLE");
        assert!(e.contains("lowercase"), "{e}");
        assert!(e.ends_with("Write \"banned.example\"."), "{e}");
        let e = key_err(".Example.com");
        assert!(e.ends_with("Write \".example.com\"."), "{e}");
        let e = key_err("example.com.");
        assert!(e.contains("trailing dot"), "{e}");
        assert!(e.ends_with("write \"example.com\"."), "{e}");
        let e = key_err("https://.example.com..");
        assert!(e.ends_with("write \"https://.example.com\"."), "{e}");
        let e = key_err("example%2ecom");
        assert!(e.ends_with("Write \"example.com\"."), "{e}");
    }

    #[test]
    fn ip_literals_must_be_canonical() {
        let e = key_err("0x7f.1");
        assert!(e.contains("canonical form"), "{e}");
        assert!(e.ends_with("Write \"127.0.0.1\"."), "{e}");
        let e = key_err("[2001:DB8:0::1]");
        assert!(e.contains("canonical form"), "{e}");
        assert!(e.ends_with("Write \"[2001:db8::1]\"."), "{e}");
        let e = key_err("2001:db8::1");
        assert!(e.contains("brackets"), "{e}");
        assert!(e.ends_with("Write \"[2001:db8::1]\"."), "{e}");
        let e = key_err(".127.0.0.1");
        assert!(e.contains("no \".\" prefix"), "{e}");
    }

    #[test]
    fn malformed_hosts_are_load_errors() {
        for key in [
            "",
            ".",
            "https://",
            "..example.com",
            "a..b.com",
            "user@example.com",
            "exa mple.com",
        ] {
            let e = key_err(key);
            assert!(e.starts_with("config error in [rules.url."), "{key}: {e}");
        }
        assert!(key_err("user:pw@example.com").ends_with("Write \"example.com\"."));
    }

    #[test]
    fn normal_form_keys_compile() {
        let set = compile(vec![
            ("example.com", allow()),
            (".example.org", allow()),
            ("https://example.net", allow()),
            ("http://.example.net", allow()),
            ("localhost", allow()),
            ("127.0.0.1", allow()),
            ("[2001:db8::1]", allow()),
            ("xn--bcher-kva.de", allow()),
        ]);
        assert_eq!(set.len(), 8);
        let r = set.eval(&url("http://example.net/")).unwrap();
        assert_eq!(
            (
                r.key.as_str(),
                r.host.as_str(),
                r.exact,
                r.scheme.as_deref()
            ),
            ("http://.example.net", "example.net", true, Some("http"))
        );
    }

    #[test]
    fn deny_needs_a_non_blank_message() {
        for message in [None, Some(""), Some("   "), Some("\n\t")] {
            let rule = UrlRule {
                action: RuleAction::Deny,
                message: message.map(str::to_string),
                ..UrlRule::default()
            };
            let e = RuleSet::compile(&section(vec![("example.com", rule)])).unwrap_err();
            assert!(e.starts_with("config error in [rules.url.\"example.com\"]: a \"deny\" rule needs a non-blank message"), "{e}");
        }
        // The negative case: an allow rule needs none.
        assert!(RuleSet::compile(&section(vec![("example.com", allow())])).is_ok());
    }

    #[test]
    fn reason_must_be_lowercase_snake() {
        for reason in ["IP-Ban", "ip.ban", "", "ban!"] {
            let rule = UrlRule {
                reason: Some(reason.to_string()),
                ..deny("go elsewhere")
            };
            let e = RuleSet::compile(&section(vec![("example.com", rule)])).unwrap_err();
            assert!(
                e.contains("may contain only lowercase letters"),
                "{reason}: {e}"
            );
        }
        let e = RuleSet::compile(&section(vec![(
            "example.com",
            UrlRule {
                reason: Some("IP-Ban".into()),
                ..deny("go elsewhere")
            },
        )]))
        .unwrap_err();
        assert!(e.contains("reason = \"ip_ban\""), "{e}");
        let ok = UrlRule {
            reason: Some("ip_ban_2".into()),
            ..deny("go elsewhere")
        };
        assert!(RuleSet::compile(&section(vec![("example.com", ok)])).is_ok());
    }

    #[test]
    fn fields_that_do_not_apply_to_the_action_are_ignored() {
        // A deny overridden to allow by a higher layer keeps the deny's
        // fields after the merge; they must not fail the load.
        let merged = UrlRule {
            action: RuleAction::Allow,
            message: Some("  ".into()),
            reason: Some("Not.Valid".into()),
            kind: DenyKind::Permanent,
            ..UrlRule::default()
        };
        let set = compile(vec![("example.com", merged)]);
        assert_eq!(set.denial(&url("https://example.com/")), None);
    }

    #[test]
    fn duplicate_patterns_are_a_load_error_disabled_ones_included() {
        let rule = |key: &str, enabled: bool| CompiledRule {
            key: key.to_string(),
            host: "example.com".to_string(),
            exact: false,
            scheme: None,
            rule: UrlRule {
                enabled,
                ..UrlRule::default()
            },
        };
        let e =
            check_duplicates(&[rule("Example.com", false), rule("example.com", true)]).unwrap_err();
        assert_eq!(
            e,
            "config error: [rules.url.\"Example.com\"] and [rules.url.\"example.com\"] are the same pattern.\n\
             Keep one. To override a rule from another layer, use its exact key."
        );
        // The negative case: the exact form, or one scheme, is another pattern.
        let mut exact = rule(".example.com", true);
        exact.exact = true;
        let mut https = rule("https://example.com", true);
        https.scheme = Some("https".into());
        assert!(check_duplicates(&[rule("example.com", true), exact, https]).is_ok());
    }

    #[test]
    fn unknown_fields_and_actions_are_rejected_by_serde() {
        let e = serde_json::from_value::<UrlRule>(serde_json::json!({"actoin": "deny"}));
        assert!(e.is_err());
        let e = serde_json::from_value::<UrlRule>(serde_json::json!({"action": "warn"}));
        assert!(e.is_err());
        let e = serde_json::from_value::<RulesSection>(serde_json::json!({"rule": {}}));
        assert!(e.is_err());
        let e = serde_json::from_value::<UrlRule>(serde_json::json!({"tier": "3"}));
        assert!(e.is_err());
        // The negative case: every known field, spelled as in the TOML.
        let r: UrlRule = serde_json::from_value(serde_json::json!({
            "enabled": true, "action": "deny", "message": "m", "kind": "permanent",
            "reason": "ip_ban", "tier": "2", "tier_enforce": true
        }))
        .unwrap();
        assert_eq!(r.tier, Some(RuleTier::Two));
        assert_eq!(r.kind, DenyKind::Permanent);
        let s: RulesSection = serde_json::from_value(serde_json::json!({"mode": "off"})).unwrap();
        assert_eq!(s.mode, RulesMode::Off);
        assert_eq!(s.crawl_denied_urls_per_rule, 20);
        // "auto" is a valid tier, advisory or enforced.
        let r: UrlRule = serde_json::from_value(serde_json::json!({"tier": "auto"})).unwrap();
        assert_eq!(r.tier, Some(RuleTier::Auto));
        assert!(
            RuleSet::compile(&RulesSection {
                url: [("example.com".to_string(), r)].into_iter().collect(),
                ..Default::default()
            })
            .is_ok()
        );
    }

    #[test]
    fn defaults_match_the_design() {
        let s = RulesSection::default();
        assert_eq!(s.mode, RulesMode::Enforce);
        assert_eq!(s.crawl_denied_urls_per_rule, 20);
        assert!(s.url.is_empty());
        let r = UrlRule::default();
        assert!(r.enabled);
        assert_eq!(r.action, RuleAction::Allow);
        assert_eq!(r.kind, DenyKind::Walled);
        assert!(!r.tier_enforce);
        assert!(RuleSet::compile(&s).unwrap().is_empty());
    }

    #[test]
    fn mode_off_still_compiles_and_validates() {
        let mut s = section(vec![("example.com:80", allow())]);
        s.mode = RulesMode::Off;
        assert!(RuleSet::compile(&s).is_err());
    }

    // --- matching ----------------------------------------------------------

    #[test]
    fn bare_host_covers_subdomains_on_label_boundaries() {
        let set = compile(vec![("banned.example", deny("use a browser"))]);
        for u in [
            "https://banned.example/",
            "http://www.banned.example/profile/x?q=1",
            "https://sub.www.banned.example/",
            "https://banned.example:8443/",
        ] {
            assert_eq!(winner(&set, u), Some("banned.example"), "{u}");
        }
        // The negative cases: a shared suffix that is not a label boundary,
        // and a host the key is a subdomain of.
        assert_eq!(winner(&set, "https://notbanned.example/"), None);
        assert_eq!(winner(&set, "https://net/"), None);
        assert_eq!(winner(&set, "https://banned.example.evil.com/"), None);
    }

    #[test]
    fn exact_key_matches_that_host_only() {
        let set = compile(vec![(".example.com", deny("no"))]);
        assert_eq!(winner(&set, "https://example.com/x"), Some(".example.com"));
        assert_eq!(winner(&set, "https://www.example.com/x"), None);
    }

    #[test]
    fn carve_out_pair_allows_the_apex_only() {
        let set = compile(vec![("example.com", deny("no")), (".example.com", allow())]);
        assert_eq!(set.denial(&url("https://example.com/")), None);
        assert_eq!(winner(&set, "https://example.com/"), Some(".example.com"));
        let d = set.denial(&url("https://www.example.com/")).unwrap();
        assert_eq!(d.rule, "example.com");
    }

    #[test]
    fn scheme_form_restricts_to_one_scheme() {
        let set = compile(vec![("http://example.com", deny("use https"))]);
        assert!(set.denial(&url("http://www.example.com/")).is_some());
        assert_eq!(set.denial(&url("https://www.example.com/")), None);
        // One scheme beats both at the same host.
        let set = compile(vec![
            ("example.com", allow()),
            ("https://example.com", deny("no")),
        ]);
        assert_eq!(
            winner(&set, "https://example.com/"),
            Some("https://example.com")
        );
        assert_eq!(winner(&set, "http://example.com/"), Some("example.com"));
    }

    #[test]
    fn trailing_dot_url_cannot_evade_a_deny() {
        let set = compile(vec![("banned.example", deny("no"))]);
        for u in [
            "https://banned.example./profile",
            "https://www.banned.example../x",
            "https://banned.example%2e/x",
        ] {
            assert!(set.denial_for_str(u).is_some(), "{u}");
        }
    }

    #[test]
    fn uppercase_and_idn_url_hosts_match_normal_keys() {
        let set = compile(vec![
            ("banned.example", deny("no")),
            ("xn--bcher-kva.de", deny("no")),
        ]);
        assert!(set.denial_for_str("https://Banned.EXAMPLE/x").is_some());
        assert!(set.denial_for_str("https://www.bücher.de/").is_some());
        assert!(set.denial_for_str("https://bucher.de/").is_none());
    }

    #[test]
    fn ip_literals_match_that_address_only() {
        let set = compile(vec![
            ("127.0.0.1", deny("no")),
            ("[2001:db8::1]", deny("no")),
        ]);
        assert!(set.denial_for_str("http://0x7f.1/x").is_some());
        assert!(set.denial_for_str("http://127.0.0.1:8080/").is_some());
        assert!(set.denial_for_str("https://[2001:DB8::1]:8080/x").is_some());
        assert!(set.denial_for_str("http://127.0.0.2/").is_none());
        assert!(set.denial_for_str("https://[2001:db8::2]/").is_none());
    }

    #[test]
    fn disabled_rule_falls_through_to_the_broader_rule() {
        let off = UrlRule {
            enabled: false,
            ..allow()
        };
        let set = compile(vec![("example.com", deny("no")), ("www.example.com", off)]);
        assert_eq!(
            winner(&set, "https://www.example.com/"),
            Some("example.com")
        );
        // It still shows among the matches, ahead of the rule that won.
        let keys: Vec<&str> = set
            .matches(&url("https://www.example.com/"))
            .iter()
            .map(|r| r.key.as_str())
            .collect();
        assert_eq!(keys, ["www.example.com", "example.com"]);
        // A disabled rule alone matches nothing.
        let set = compile(vec![(
            "example.com",
            UrlRule {
                enabled: false,
                ..deny("no")
            },
        )]);
        assert_eq!(set.denial(&url("https://example.com/")), None);
    }

    #[test]
    fn narrow_tier_only_rule_replaces_a_broader_deny() {
        let tier_only = UrlRule {
            tier: Some(RuleTier::Two),
            ..UrlRule::default()
        };
        let set = compile(vec![
            ("banned.example", deny("use a browser")),
            ("www.banned.example", tier_only),
        ]);
        let won = set.eval(&url("https://www.banned.example/x")).unwrap();
        assert_eq!(won.key, "www.banned.example");
        assert_eq!(won.rule.action, RuleAction::Allow);
        assert_eq!(won.rule.tier, Some(RuleTier::Two));
        assert_eq!(set.denial(&url("https://www.banned.example/x")), None);
        // The negative case: the apex is still denied.
        assert!(set.denial(&url("https://banned.example/x")).is_some());
    }

    #[test]
    fn matches_are_in_precedence_order() {
        let set = compile(vec![
            ("example.com", allow()),
            ("http://example.com", allow()),
            ("www.example.com", allow()),
            ("https://www.example.com", allow()),
            (".www.example.com", allow()),
            ("https://.www.example.com", allow()),
            ("other.com", allow()),
        ]);
        let keys: Vec<&str> = set
            .matches(&url("https://www.example.com/"))
            .iter()
            .map(|r| r.key.as_str())
            .collect();
        assert_eq!(
            keys,
            [
                "https://.www.example.com",
                ".www.example.com",
                "https://www.example.com",
                "www.example.com",
                "example.com",
            ]
        );
    }

    #[test]
    fn empty_set_and_hostless_urls_match_nothing() {
        let empty = RuleSet::empty();
        assert!(empty.is_empty());
        assert_eq!(empty.eval(&url("https://example.com/")), None);
        let set = compile(vec![("example.com", deny("no"))]);
        assert!(set.matches(&url("mailto:a@example.com")).is_empty());
        assert_eq!(set.denial_for_str("not a url"), None);
    }

    // --- the denial payload ------------------------------------------------

    #[test]
    fn denial_code_has_three_segments() {
        let set = compile(vec![
            (
                "a.com",
                UrlRule {
                    reason: Some("ip_ban".into()),
                    kind: DenyKind::Permanent,
                    ..deny("ask the operator")
                },
            ),
            ("b.com", deny("ask the operator")),
        ]);
        let a = set.denial_for_str("https://a.com/").unwrap();
        assert_eq!(a.code(), "policy.denied.ip_ban");
        assert_eq!(a.kind, "permanent");
        assert_eq!(a.kind_static(), "permanent");
        assert_eq!(a.message, "ask the operator");
        assert_eq!(a.rule, "a.com");
        let b = set.denial_for_str("https://b.com/").unwrap();
        assert_eq!(b.code(), "policy.denied.unspecified");
        assert_eq!(b.kind_static(), "walled");
        assert_eq!(policy_code(None), "policy.denied.unspecified");
    }

    #[test]
    fn denial_round_trips_through_fetch_error() {
        let d = Denial {
            rule: "example.com".into(),
            message: "go elsewhere".into(),
            kind: "permanent".into(),
            reason: Some("licensing".into()),
        };
        let e = d.clone().into_error();
        assert_eq!(Denial::from_error(&e), Some(d));
        assert_eq!(Denial::from_error(&FetchError::Timeout), None);
    }

    #[test]
    fn kind_phrases() {
        assert_eq!(DenyKind::Walled.phrase(), "walled: try another source");
        assert_eq!(DenyKind::Permanent.phrase(), "permanent: do not pursue");
        assert_eq!(kind_phrase("permanent"), "permanent: do not pursue");
        assert_eq!(kind_phrase("walled"), "walled: try another source");
        assert_eq!(kind_phrase("anything"), "walled: try another source");
        assert_eq!(RulesMode::Off.as_str(), "off");
        assert_eq!(RuleTier::One.as_str(), "1");
        assert_eq!(RuleTier::Auto.as_str(), "auto");
    }
}
