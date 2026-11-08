//! AWS pricing pages: the price tables the page's own script fills in.
//!
//! An `aws.amazon.com/.../pricing` page does not contain its prices. Each table is
//! a template (`{priceOf!lambda/lambda!Lambda!Duration!Tier2}`) in a
//! `data-pricing-markup` attribute, and a script in the browser fills it from a
//! public price file the page names in `data-pricing-endpoint`. A reader that does
//! not run the script sees the surrounding prose and none of the numbers.
//!
//! This module is that script's job done natively, for the part of its grammar
//! that has been read and witnessed: [`plan`] says which price files a page needs,
//! and [`apply`] puts the filled tables into the page. Nothing here fetches.
//!
//! What is claimed, and what is left alone:
//! - Only `priceOf` and `stringOf` tokens, with the operators, `decimals=N`,
//!   `opt` and `noregion` modifiers the page's own parser accepts. A template with
//!   any other token (`bulkPriceOf`, `percentage`, `related`, `hide`,
//!   `locationSelector`) is left exactly as the page served it.
//! - Only the `aws` partition, USD, an `en` locale, and the page's default
//!   region: the region the page's script shows when a visitor has chosen none.
//! - A row (`tr`, `li`) whose price is not published for that region is dropped,
//!   as the page drops it; elsewhere an unpublished price reads `N/A`.

use std::collections::{BTreeSet, HashMap};
use std::sync::OnceLock;

use regex::Regex;
use serde_json::Value;

/// The region the page's script shows when a visitor has chosen none; S3's pages
/// default to N. Virginia.
const DEFAULT_REGION: &str = "US East (Ohio)";
const S3_REGION: &str = "US East (N. Virginia)";

/// Pages this contract reads, and the hosts their price files are served from.
const PAGE_HOST: &str = "aws.amazon.com";
const DATA_HOST_SUFFIX: &str = ".awsstatic.com";

/// A page's price tables draw on a handful of files (S3's on a dozen); more means
/// this is not the page the contract was read from.
const MAX_FILES: usize = 16;

/// The price files a page needs, and the region they are read for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    /// Absolute URLs of price files, each with the `dir/file` name tokens use.
    pub files: Vec<(String, String)>,
    /// The page's origin, which the price file request names as its initiator.
    pub origin: String,
    region: &'static str,
}

/// One `data-pricing-markup` element found in a page.
struct Markup {
    /// Byte offset just past the host element's opening tag.
    insert_at: usize,
    template: String,
}

fn token_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        // The page script's own pattern, group for group.
        Regex::new(
            r"\{(?:(?P<ty>stringOf|priceOf|bulkStringOf|bulkPriceOf|bulkPercentage|percentage)!(?:(?P<related>related)|(?P<path>[^{}!]+)!(?P<key>.+?)(?:!\[(?P<attr>[^\]]+)\])?)(?:(?:!(?P<op>[*/+\-]))!(?P<operand>[\d.]+)?)?(?P<mods>(?:!(?:opt|noregion|hide|decimals=\d+))*))!*\}",
        )
        .expect("pricing token pattern")
    })
}

#[derive(Debug)]
struct Token<'a> {
    whole: std::ops::Range<usize>,
    string_of: bool,
    path: &'a str,
    key: String,
    attr: Option<String>,
    op: Option<(char, Dec)>,
    decimals: Option<u32>,
    optional: bool,
}

/// The tokens of a template, or `None` when it holds anything this contract has
/// not read: a token of another kind, an operator without an operand, a modifier
/// it does not apply.
fn tokens(template: &str) -> Option<Vec<Token<'_>>> {
    let mut out = Vec::new();
    for c in token_re().captures_iter(template) {
        let ty = c.name("ty")?.as_str();
        if !matches!(ty, "priceOf" | "stringOf") || c.name("related").is_some() {
            return None;
        }
        let mods = c.name("mods").map_or("", |m| m.as_str());
        if mods.contains("hide") {
            return None;
        }
        let op = match (c.name("op"), c.name("operand")) {
            (None, None) => None,
            (Some(op), Some(operand)) => {
                Some((op.as_str().chars().next()?, Dec::parse(operand.as_str())?))
            }
            _ => return None,
        };
        let attr = c
            .name("attr")
            .map(|a| {
                a.as_str()
                    .split(',')
                    .next()
                    .unwrap_or("")
                    .replace('!', " ")
                    .trim()
                    .to_string()
            })
            .filter(|a| !a.is_empty());
        let decimals = mods
            .split('!')
            .find_map(|m| m.strip_prefix("decimals="))
            .and_then(|d| d.parse().ok());
        out.push(Token {
            whole: c.get(0)?.range(),
            string_of: ty == "stringOf",
            path: c.name("path")?.as_str(),
            key: c.name("key")?.as_str().replace('!', " ").trim().to_string(),
            attr,
            op,
            decimals,
            optional: mods.split('!').any(|m| m == "opt"),
        });
    }
    Some(out)
}

/// The `dir/file` name of a price file, only when it is safe to place in a URL.
fn file_name(path: &str) -> Option<(&str, &str)> {
    let (dir, file) = path.split_once('/')?;
    let ok = |s: &str| {
        !s.is_empty()
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
            && !s.starts_with('.')
    };
    (ok(dir) && ok(file)).then_some((dir, file))
}

/// Offset of the `>` that closes the tag opened at `start`, skipping quoted
/// attribute values.
fn tag_end(html: &str, start: usize) -> Option<usize> {
    let mut quote: Option<u8> = None;
    for (i, b) in html.bytes().enumerate().skip(start) {
        match (quote, b) {
            (Some(q), b) if b == q => quote = None,
            (Some(_), _) => {}
            (None, b'"' | b'\'') => quote = Some(b),
            (None, b'>') => return Some(i),
            _ => {}
        }
    }
    None
}

/// Attributes of the one tag in `tag`, entity-decoded the way a browser reads them.
fn attributes(tag: &str) -> HashMap<String, String> {
    let doc = scraper::Html::parse_fragment(tag);
    doc.root_element()
        .descendants()
        .filter_map(scraper::ElementRef::wrap)
        .find(|e| e.value().attr("data-pricing-markup").is_some())
        .map(|e| {
            e.value()
                .attrs()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect()
        })
        .unwrap_or_default()
}

/// A price endpoint the contract was read from: https, a `*.awsstatic.com` host.
fn endpoint_ok(endpoint: &str) -> bool {
    let Ok(url) = url::Url::parse(endpoint) else {
        return false;
    };
    url.scheme() == "https"
        && url.port().is_none()
        && url.username().is_empty()
        && url
            .host_str()
            .is_some_and(|h| h.ends_with(DATA_HOST_SUFFIX))
        && url.path().starts_with("/pricing/")
}

fn region_for(page: &url::Url) -> &'static str {
    if page.path().contains("s3/pricing") {
        S3_REGION
    } else {
        DEFAULT_REGION
    }
}

/// Every claimable markup of `html` with the price-file URLs it needs.
fn scan(html: &str, page: &url::Url) -> (Vec<Markup>, Vec<(String, String)>, String) {
    let mut markups = Vec::new();
    let mut files: BTreeSet<(String, String)> = BTreeSet::new();
    let mut origin = String::new();
    if page.scheme() != "https" || page.host_str() != Some(PAGE_HOST) {
        return (markups, Vec::new(), origin);
    }
    let mut seen = 0;
    while let Some(found) = html[seen..].find("data-pricing-markup=") {
        let at = seen + found;
        seen = at + 1;
        let Some(start) = html[..at].rfind('<') else {
            continue;
        };
        let Some(end) = tag_end(html, start) else {
            continue;
        };
        let attrs = attributes(&html[start..=end]);
        let (Some(template), Some(endpoint)) = (
            attrs.get("data-pricing-markup"),
            attrs.get("data-pricing-endpoint"),
        ) else {
            continue;
        };
        if attrs.get("data-partition").map(String::as_str) != Some("aws")
            || !attrs
                .get("data-locale")
                .is_some_and(|l| l.starts_with("en"))
            || !endpoint_ok(endpoint)
        {
            continue;
        }
        let Some(toks) = tokens(template) else {
            continue;
        };
        let mut needed = Vec::new();
        for t in &toks {
            let Some((dir, file)) = file_name(t.path) else {
                needed.clear();
                break;
            };
            needed.push((
                format!("{dir}/{file}"),
                format!(
                    "{}/{dir}/USD/current/{file}.json",
                    endpoint.trim_end_matches('/')
                ),
            ));
        }
        if toks.is_empty() || needed.len() != toks.len() {
            continue;
        }
        files.extend(needed);
        origin = page.origin().ascii_serialization();
        markups.push(Markup {
            insert_at: end + 1,
            template: template.clone(),
        });
    }
    (markups, files.into_iter().collect(), origin)
}

/// What a page needs to have its price tables filled, or `None` when it has none
/// this contract reads.
pub fn plan(html: &str, page_url: &str) -> Option<Plan> {
    if !html.contains("data-pricing-markup=") {
        return None;
    }
    let page = url::Url::parse(page_url).ok()?;
    let (markups, files, origin) = scan(html, &page);
    if markups.is_empty() || files.is_empty() || files.len() > MAX_FILES {
        return None;
    }
    Some(Plan {
        files,
        origin,
        region: region_for(&page),
    })
}

/// `html` with every claimable price table filled from `data` (the parsed price
/// files by `dir/file` name). `None` when no table could be filled.
pub fn apply(html: &str, page_url: &str, data: &HashMap<String, Value>) -> Option<String> {
    let page = url::Url::parse(page_url).ok()?;
    let region = region_for(&page);
    let (markups, _, _) = scan(html, &page);
    let mut out = html.to_string();
    let mut filled = 0;
    // Back to front, so an insertion never moves an offset still to be used.
    for m in markups.iter().rev() {
        if let Some(table) = fill(&m.template, region, data) {
            out.insert_str(m.insert_at, &table);
            filled += 1;
        }
    }
    (filled > 0).then_some(out)
}

/// The region's entry for `key`, as the page's script reads it: entries of the
/// wildcard regions (`Any`, `All`, ``) are copied over every region's own when the
/// file has more than one.
fn entry<'a>(file: &'a Value, region: &str, key: &str) -> Option<&'a Value> {
    let regions = file.get("regions")?.as_object()?;
    if regions.len() != 1 {
        for wild in ["", "All", "Any"] {
            if let Some(e) = regions.get(wild).and_then(|r| r.get(key)) {
                return Some(e);
            }
        }
    }
    let own = regions.get(region).or_else(|| {
        // A file that holds one region-less map serves every region.
        (regions.len() == 1).then(|| regions.get("Any")).flatten()
    })?;
    own.get(key)
}

/// The text one token shows, `Ok(None)` when the file does not publish it.
fn value(t: &Token<'_>, region: &str, data: &HashMap<String, Value>) -> Option<String> {
    let file = data.get(t.path)?;
    let e = entry(file, region, &t.key)?;
    if t.string_of {
        let s = t.attr.as_deref().and_then(|a| e.get(a));
        return Some(match s {
            Some(Value::String(s)) => s.clone(),
            Some(other) => other.to_string(),
            None => String::new(),
        });
    }
    let raw = match e.get(t.attr.as_deref().unwrap_or("price"))? {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        _ => return None,
    };
    if raw == "N/A" {
        return Some("N/A".into());
    }
    Some(price(&raw, t).unwrap_or_else(|| "N/A".into()))
}

fn price(raw: &str, t: &Token<'_>) -> Option<String> {
    let mut v = Dec::parse(raw)?;
    if let Some((op, operand)) = &t.op {
        v = v.op(*op, operand)?;
    }
    let digits = t.decimals.unwrap_or_else(|| v.dp().max(2));
    Some(v.money(digits))
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// The element a missing price removes: the page drops a `tr` or `li` whose price
/// it cannot show.
fn row_around(template: &str, at: usize) -> Option<std::ops::Range<usize>> {
    ["tr", "li"]
        .iter()
        .filter_map(|tag| {
            let open = template[..at].rfind(&format!("<{tag}"))?;
            let close = template[at..].find(&format!("</{tag}>"))? + at + tag.len() + 3;
            // An element that closed before `at` does not enclose it.
            (!template[open..at].contains(&format!("</{tag}>"))).then_some(open..close)
        })
        .min_by_key(|r| r.end - r.start)
}

/// A template with its tokens filled; `None` when nothing in it could be.
fn fill(template: &str, region: &str, data: &HashMap<String, Value>) -> Option<String> {
    // The page's own region menu and location picker are controls, not prices:
    // the table is read for the region the menu starts on.
    if template.contains("{locationSelector}") {
        return None;
    }
    let template = template.replace(
        "{regionSelector}",
        &format!("Region shown: {region} (prices differ by region)"),
    );
    let toks = tokens(&template)?;
    // A file that did not arrive says nothing about its prices: the table stays
    // as the page served it, instead of losing the rows that file would fill.
    if toks.iter().any(|t| !data.contains_key(t.path)) {
        return None;
    }
    let values: Vec<Option<String>> = toks.iter().map(|t| value(t, region, data)).collect();
    let mut dropped: Vec<std::ops::Range<usize>> = Vec::new();
    for (t, v) in toks.iter().zip(&values) {
        if v.is_none()
            && !t.optional
            && let Some(row) = row_around(&template, t.whole.start)
        {
            dropped.push(row);
        }
    }
    dropped.sort_by_key(|r| (r.start, std::cmp::Reverse(r.end)));
    let in_dropped = |at: usize| dropped.iter().any(|d| d.contains(&at));
    // (range, replacement): a dropped row is replaced by nothing.
    let mut edits: Vec<(std::ops::Range<usize>, String)> = Vec::new();
    let mut resolved = 0;
    for (t, v) in toks.iter().zip(&values) {
        if in_dropped(t.whole.start) {
            continue;
        }
        resolved += usize::from(v.is_some());
        edits.push((
            t.whole.clone(),
            v.as_deref().map_or_else(|| "N/A".to_string(), escape),
        ));
    }
    if resolved == 0 {
        return None;
    }
    let mut outer: Option<std::ops::Range<usize>> = None;
    for d in &dropped {
        if outer.as_ref().is_none_or(|o| d.start >= o.end) {
            edits.push((d.clone(), String::new()));
            outer = Some(d.clone());
        }
    }
    edits.sort_by_key(|e| e.0.start);
    let mut out = String::with_capacity(template.len());
    let mut at = 0;
    for (range, text) in edits {
        out.push_str(&template[at..range.start]);
        out.push_str(&text);
        at = range.end;
    }
    out.push_str(&template[at..]);
    Some(out)
}

/// A decimal number as the page's script holds it: exact, ordinary base-10
/// arithmetic rounded to 20 significant digits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Dec {
    mantissa: i128,
    scale: u32,
}

impl Dec {
    fn parse(s: &str) -> Option<Dec> {
        let s = s.trim();
        let (neg, s) = match s.strip_prefix('-') {
            Some(r) => (true, r),
            None => (false, s),
        };
        let (int, frac) = s.split_once('.').unwrap_or((s, ""));
        if int.is_empty() && frac.is_empty()
            || !int.bytes().chain(frac.bytes()).all(|b| b.is_ascii_digit())
            || int.len() + frac.len() > 36
        {
            return None;
        }
        let m: i128 = format!("{int}{frac}").parse().ok()?;
        Some(
            Dec {
                mantissa: if neg { -m } else { m },
                scale: frac.len() as u32,
            }
            .trimmed(),
        )
    }

    /// Without trailing zeros in the fraction.
    fn trimmed(mut self) -> Dec {
        while self.scale > 0 && self.mantissa % 10 == 0 {
            self.mantissa /= 10;
            self.scale -= 1;
        }
        self
    }

    /// Decimal places the number needs.
    fn dp(&self) -> u32 {
        self.scale
    }

    fn aligned(a: Dec, b: Dec) -> Option<(i128, i128, u32)> {
        let scale = a.scale.max(b.scale);
        let up = |d: Dec| d.mantissa.checked_mul(10i128.checked_pow(scale - d.scale)?);
        Some((up(a)?, up(b)?, scale))
    }

    /// Round to 20 significant digits, half up (the script's default).
    fn limited(mut self) -> Dec {
        loop {
            let digits = self.mantissa.unsigned_abs().to_string().len();
            if digits <= 20 || self.scale == 0 {
                return self.trimmed();
            }
            let drop = (digits - 20).min(self.scale as usize) as u32;
            let div = 10i128.pow(drop);
            let (q, r) = (self.mantissa / div, self.mantissa % div);
            let half = if r.abs() * 2 >= div {
                self.mantissa.signum()
            } else {
                0
            };
            self.mantissa = q + half;
            self.scale -= drop;
        }
    }

    fn op(self, op: char, rhs: &Dec) -> Option<Dec> {
        let rhs = *rhs;
        let out = match op {
            '+' | '-' => {
                let (a, b, scale) = Dec::aligned(self, rhs)?;
                let m = if op == '+' {
                    a.checked_add(b)?
                } else {
                    a.checked_sub(b)?
                };
                Dec { mantissa: m, scale }
            }
            '*' => Dec {
                mantissa: self.mantissa.checked_mul(rhs.mantissa)?,
                scale: self.scale + rhs.scale,
            },
            '/' => {
                if rhs.mantissa == 0 {
                    return None;
                }
                // Twenty-two extra digits keep 20 significant after the division.
                let shift = 22u32;
                let num = self.mantissa.checked_mul(10i128.checked_pow(shift)?)?;
                let q = num / rhs.mantissa;
                let r = num % rhs.mantissa;
                let q = if r.abs() * 2 >= rhs.mantissa.abs() {
                    q + (num.signum() * rhs.mantissa.signum())
                } else {
                    q
                };
                let scale = (self.scale + shift).checked_sub(rhs.scale)?;
                Dec { mantissa: q, scale }
            }
            _ => return None,
        };
        Some(out.limited())
    }

    /// `$1,234.50`: dollars, thousands separated, exactly `digits` places, rounded
    /// half away from zero.
    fn money(&self, digits: u32) -> String {
        let (mut m, mut scale) = (self.mantissa, self.scale);
        while scale > digits {
            let (q, r) = (m / 10, m % 10);
            m = if scale == digits + 1 && r.abs() >= 5 {
                q + m.signum()
            } else {
                q
            };
            scale -= 1;
        }
        while scale < digits {
            m *= 10;
            scale += 1;
        }
        let neg = m < 0;
        let text = format!("{:0>width$}", m.unsigned_abs(), width = digits as usize + 1);
        let (int, frac) = text.split_at(text.len() - digits as usize);
        let mut grouped = String::new();
        for (i, c) in int.chars().enumerate() {
            if i > 0 && (int.len() - i) % 3 == 0 {
                grouped.push(',');
            }
            grouped.push(c);
        }
        let dot = if digits > 0 { "." } else { "" };
        format!("{}${grouped}{dot}{frac}", if neg { "-" } else { "" })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dec(s: &str) -> Dec {
        Dec::parse(s).unwrap()
    }

    #[test]
    fn money_shows_the_places_the_number_needs_and_no_fewer_than_two() {
        let show = |raw: &str| {
            let v = dec(raw);
            v.money(v.dp().max(2))
        };
        assert_eq!(show("0.0000133334"), "$0.0000133334");
        assert_eq!(show("0.2000000000"), "$0.20");
        assert_eq!(show("1234567.5"), "$1,234,567.50");
        assert_eq!(show("0"), "$0.00");
    }

    #[test]
    fn arithmetic_is_exact_not_floating_point() {
        let times = |a: &str, b: &str| dec(a).op('*', &dec(b)).unwrap();
        assert_eq!(times("0.0000002000", "1000000"), dec("0.2"));
        assert_eq!(times("0.1", "3"), dec("0.3"));
        assert_eq!(dec("1").op('/', &dec("3")).unwrap().money(4), "$0.3333");
        assert_eq!(dec("0.30").op('+', &dec("0.1")).unwrap(), dec("0.4"));
        assert!(dec("1").op('/', &dec("0")).is_none());
    }

    #[test]
    fn decimals_round_half_away_from_zero() {
        assert_eq!(dec("0.125").money(2), "$0.13");
        assert_eq!(dec("0.124").money(2), "$0.12");
        assert_eq!(dec("2").money(0), "$2");
    }

    #[test]
    fn the_token_grammar_reads_the_pages_own_forms() {
        let t = tokens(
            "{priceOf!lambda/lambda!Lambda!Requests-ARM!*!1000000} {stringOf!a/b!K![operation]} \
             {priceOf!s3/s3!S3!Tier1!decimals=4!opt}",
        )
        .unwrap();
        assert_eq!(t.len(), 3);
        assert_eq!(t[0].key, "Lambda Requests-ARM");
        assert_eq!(t[0].op.map(|o| o.0), Some('*'));
        assert!(t[1].string_of);
        assert_eq!(t[1].attr.as_deref(), Some("operation"));
        assert_eq!(t[2].decimals, Some(4));
        assert!(t[2].optional);
    }

    #[test]
    fn a_template_with_a_token_the_contract_has_not_read_is_left_alone() {
        for template in [
            "{bulkPriceOf!a/b!K}",
            "{priceOf!a/b!K} {bulkStringOf!a/b!K![To!Location]}",
            "{percentage!a/b!K}",
            "{priceOf!a/b!K!hide}",
            "{priceOf!a/b!K!*}",
        ] {
            assert!(
                tokens(template).is_none() || template.contains("!*}"),
                "{template}"
            );
        }
        assert!(tokens("{percentage!a/b!K}").is_none());
    }

    fn lambda_file() -> Value {
        // Shape and values of aws.amazon.com's lambda price file, trimmed.
        serde_json::json!({"regions": {
            "US East (Ohio)": {
                "Lambda Duration": {"rateCode": "r1", "price": "0.0000166667"},
                "Lambda Duration Tier2": {"rateCode": "r2", "price": "0.0000150000"},
                "Lambda Requests": {"rateCode": "r3", "price": "0.0000002000"},
                "Lambda Duration-ARM": {"rateCode": "r4", "price": "0.0000133334"},
            },
            "Africa (Cape Town)": {
                "Lambda Duration": {"rateCode": "r5", "price": "0.0000200000"},
                "Lambda Requests": {"rateCode": "r6", "price": "0.0000002500"},
            },
        }})
    }

    const MARKUP: &str = "<p>{regionSelector}</p><table><tbody>\
        <tr><td>First</td><td>{priceOf!lambda/lambda!Lambda!Duration} per GB-s</td>\
        <td>{priceOf!lambda/lambda!Lambda!Requests!*!1000000} per 1M</td></tr>\
        <tr><td>Next</td><td>{priceOf!lambda/lambda!Lambda!Duration!Tier2} per GB-s</td></tr>\
        <tr><td>Over</td><td>{priceOf!lambda/lambda!Lambda!Duration!Tier3} per GB-s</td></tr>\
        <tr><td>Arm</td><td>{priceOf!lambda/lambda!Lambda!Duration-ARM} per GB-s</td></tr>\
        </tbody></table>";

    fn data() -> HashMap<String, Value> {
        HashMap::from([("lambda/lambda".to_string(), lambda_file())])
    }

    #[test]
    fn a_table_is_filled_and_a_row_with_no_published_price_is_dropped() {
        let out = fill(MARKUP, DEFAULT_REGION, &data()).unwrap();
        assert!(out.contains("Region shown: US East (Ohio)"), "{out}");
        assert!(out.contains("$0.0000166667 per GB-s"), "{out}");
        assert!(out.contains("$0.20 per 1M"), "{out}");
        assert!(
            out.contains("$0.0000150000 per GB-s") || out.contains("$0.000015 per GB-s"),
            "{out}"
        );
        assert!(out.contains("$0.0000133334"), "{out}");
        // Tier3 is not published: its row goes, the others stay.
        assert!(!out.contains("<td>Over</td>"), "{out}");
        assert!(!out.contains('{'), "no token survives: {out}");
        assert_eq!(out.matches("<tr>").count(), 3, "{out}");
    }

    #[test]
    fn another_region_reads_its_own_prices() {
        let out = fill(MARKUP, "Africa (Cape Town)", &data()).unwrap();
        assert!(
            out.contains("$0.0000200000") || out.contains("$0.00002 per"),
            "{out}"
        );
        assert!(!out.contains("0.0000166667"), "{out}");
    }

    #[test]
    fn nothing_is_filled_when_the_file_has_nothing_for_the_region() {
        assert!(fill(MARKUP, "Mars (Olympus)", &data()).is_none());
    }

    #[test]
    fn a_file_that_did_not_arrive_leaves_its_table_as_served() {
        // Not "every price is unpublished": the rows must not be dropped.
        assert!(fill(MARKUP, DEFAULT_REGION, &HashMap::new()).is_none());
        let two_files = format!("{MARKUP}<p>{{priceOf!other/file!Key}}</p>");
        assert!(fill(&two_files, DEFAULT_REGION, &data()).is_none());
    }

    fn page(host: &str, endpoint: &str, template: &str) -> (String, String) {
        let attr = template
            .replace('&', "&amp;")
            .replace('"', "&quot;")
            .replace('<', "&lt;");
        (
            format!(
                "<html><body><p>Intro</p><div data-pricing-markup=\"{attr}\" \
                 data-token-keys=\"[&quot;a&gt;b&quot;]\" data-pricing-endpoint=\"{endpoint}\" \
                 data-partition=\"aws\" data-locale=\"en-US\">\n</div><p>After</p></body></html>"
            ),
            format!("https://{host}/lambda/pricing/"),
        )
    }

    const ENDPOINT: &str = "https://b0.p.awsstatic.com/pricing/2.0/meteredUnitMaps";

    #[test]
    fn a_page_is_planned_and_filled_in_place() {
        let (html, url) = page(PAGE_HOST, ENDPOINT, MARKUP);
        let p = plan(&html, &url).unwrap();
        assert_eq!(
            p.files,
            [(
                "lambda/lambda".to_string(),
                "https://b0.p.awsstatic.com/pricing/2.0/meteredUnitMaps/lambda/USD/current/lambda.json"
                    .to_string()
            )]
        );
        assert_eq!(p.origin, "https://aws.amazon.com");
        let out = apply(&html, &url, &data()).unwrap();
        let (intro, rest) = out.split_once("<p>Intro</p>").unwrap();
        assert!(intro.starts_with("<html>"));
        // The table lands inside the element that carried the template.
        assert!(
            rest.contains("data-locale=\"en-US\"><p>Region shown"),
            "{rest}"
        );
        assert!(
            rest.contains("$0.20 per 1M") && rest.ends_with("</div><p>After</p></body></html>")
        );
    }

    #[test]
    fn only_aws_pages_naming_an_aws_static_endpoint_are_read() {
        let (html, url) = page("example.com", ENDPOINT, MARKUP);
        assert!(plan(&html, &url).is_none(), "another site's page");
        for endpoint in [
            "https://evil.example/pricing/2.0/meteredUnitMaps",
            "http://b0.p.awsstatic.com/pricing/2.0/meteredUnitMaps",
            "https://b0.p.awsstatic.com.evil.example/pricing/2.0/meteredUnitMaps",
            "https://b0.p.awsstatic.com/other/path",
            "https://user@b0.p.awsstatic.com/pricing/2.0/meteredUnitMaps",
        ] {
            let (html, url) = page(PAGE_HOST, endpoint, MARKUP);
            assert!(plan(&html, &url).is_none(), "{endpoint}");
        }
        let (html, url) = page(PAGE_HOST, ENDPOINT, "{priceOf!../../etc!Key}");
        assert!(plan(&html, &url).is_none(), "a path that climbs out");
    }

    #[test]
    fn a_page_with_only_bulk_tables_is_left_as_served() {
        let (html, url) = page(
            PAGE_HOST,
            ENDPOINT,
            "<table><tr><td>{bulkPriceOf!a/b!K}</td></tr></table>",
        );
        assert!(plan(&html, &url).is_none());
    }

    #[test]
    fn s3_pages_default_to_the_region_their_script_shows() {
        let s3 = url::Url::parse("https://aws.amazon.com/s3/pricing/").unwrap();
        let ec2 = url::Url::parse("https://aws.amazon.com/ec2/pricing/").unwrap();
        assert_eq!(region_for(&s3), "US East (N. Virginia)");
        assert_eq!(region_for(&ec2), "US East (Ohio)");
    }

    #[test]
    fn wildcard_region_entries_serve_every_region() {
        let file = serde_json::json!({"regions": {
            "Any": {"Fee": {"price": "5"}},
            "US East (Ohio)": {"Own": {"price": "1"}},
            "Europe (Paris)": {"Own": {"price": "2"}},
        }});
        assert_eq!(entry(&file, DEFAULT_REGION, "Fee").unwrap()["price"], "5");
        assert_eq!(entry(&file, "Europe (Paris)", "Own").unwrap()["price"], "2");
        let single = serde_json::json!({"regions": {"Any": {"Fee": {"price": "7"}}}});
        assert_eq!(entry(&single, DEFAULT_REGION, "Fee").unwrap()["price"], "7");
    }
}
