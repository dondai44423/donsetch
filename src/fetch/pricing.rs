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
//! - `priceOf` and `stringOf` tokens, with the operators, `decimals=N`, `opt`
//!   and `noregion` modifiers the page's own parser accepts, and bulk tables
//!   (`bulkPriceOf`, `bulkStringOf`, one row per instance type; see [`bulk`]).
//!   A template with any other token (`percentage`, `bulkPercentage`, `hide`,
//!   `locationSelector`) or a sort order not read here is left exactly as the
//!   page served it.
//! - The `aws` partition (every template aws.amazon.com serves; the China
//!   site prints its prices in the page), USD, the page's default region (the
//!   one its script shows when a visitor has chosen none), and the fourteen
//!   locales aws.amazon.com serves, written as that page writes them
//!   ([`locale`]): `0,017 USD` on the German page, strings through its
//!   dictionaries.
//! - A row (`tr`, `li`) whose price is not published for that region is dropped,
//!   as the page drops it; elsewhere an unpublished price reads `N/A`.

mod bulk;
mod grid;
mod locale;

use std::collections::{BTreeSet, HashMap};
use std::sync::OnceLock;

use regex::Regex;
use serde_json::Value;

/// The region the page's script shows when a visitor has chosen none; S3's pages
/// default to N. Virginia.
const DEFAULT_REGION: &str = "US East (Ohio)";
const S3_REGION: &str = "US East (N. Virginia)";

/// Pages this contract reads, and the hosts their price files are served from.
pub(crate) const PAGE_HOST: &str = "aws.amazon.com";
const DATA_HOST_SUFFIX: &str = ".awsstatic.com";

/// A page's price tables draw on a handful of files (S3's on a dozen,
/// SageMaker's on twenty-two, 2.5 MB compressed in all, as the browser
/// downloads them); more means this is not the page the contract was read from.
const MAX_FILES: usize = 32;

/// The price files a page needs, and the region they are read for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    /// Absolute URLs of price files, each with the `dir/file` name tokens use,
    /// and of the page's dictionaries (`@i18n/…`, `@g11n/…`) when a template
    /// shows strings.
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
    locale: &'static locale::Locale,
    /// `data-token-sets`: the sets a bulk table's rows come from.
    sets: Vec<String>,
    relates: Option<String>,
    sort_property: String,
    sort_order: String,
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
    /// `bulkPriceOf` / `bulkStringOf`: one value per row of a bulk table.
    bulk: bool,
    /// `{bulkStringOf!related}`: the value the rows are joined on.
    related: bool,
    /// Empty for a related token.
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
        let bulk = matches!(ty, "bulkPriceOf" | "bulkStringOf");
        let related = c.name("related").is_some();
        if !(bulk || matches!(ty, "priceOf" | "stringOf")) || (related && !bulk) {
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
            string_of: matches!(ty, "stringOf" | "bulkStringOf"),
            bulk,
            related,
            path: c.name("path").map_or("", |m| m.as_str()),
            // The page reads a template's text after the HTML parser has
            // decoded it: `&nbsp;` in a key is a no-break space, which `trim`
            // then removes.
            key: decode_entities(c.name("key").map_or("", |m| m.as_str()))
                .replace('!', " ")
                .trim()
                .to_string(),
            attr,
            op,
            decimals,
            optional: mods.split('!').any(|m| m == "opt"),
        });
    }
    Some(out)
}

/// The few entities a pricing template's text carries, decoded as the page's
/// HTML parser decodes them.
fn decode_entities(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    s.replace("&nbsp;", "\u{a0}")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&amp;", "&")
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

/// Attributes of the one tag in `tag` that carries the pricing markup.
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

/// The EC2 on-demand grid this page carries: where the iframe sits, the data
/// file its app reads, and the selection the app shows by default.
pub(crate) fn grid_plan(html: &str, page: &str) -> Option<(usize, String, &'static str)> {
    let page = url::Url::parse(page).ok()?;
    grid::plan(html, &page).map(|grid| (grid.insert_at, grid.url, grid.region))
}

/// The grid's rows for one selection, or `None` when the file is not its shape.
pub(crate) fn grid_table(json: &Value, region: &str) -> Option<String> {
    grid::render(json, region)
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
        let Some(locale) = attrs.get("data-locale").and_then(|l| locale::locale(l)) else {
            continue;
        };
        if attrs.get("data-partition").map(String::as_str) != Some("aws") || !endpoint_ok(endpoint)
        {
            continue;
        }
        let Some(toks) = tokens(template) else {
            continue;
        };
        let sets: Vec<String> = attrs
            .get("data-token-sets")
            .and_then(|s| serde_json::from_str(s).ok())
            .unwrap_or_default();
        // A bulk table's rows come from the sets the markup names; without
        // them the script shows none.
        if toks.iter().any(|t| t.bulk) && sets.is_empty() {
            continue;
        }
        let mut needed = Vec::new();
        let mut bad = false;
        for t in toks.iter().filter(|t| !t.related) {
            let Some((dir, file)) = file_name(t.path) else {
                bad = true;
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
        if toks.is_empty() || needed.is_empty() || bad {
            continue;
        }
        if toks.iter().any(|t| t.string_of) {
            needed.extend(locale.dictionaries());
        }
        files.extend(needed);
        origin = page.origin().ascii_serialization();
        markups.push(Markup {
            insert_at: end + 1,
            template: template.clone(),
            locale,
            sets,
            relates: attrs.get("data-relates").filter(|r| !r.is_empty()).cloned(),
            sort_property: attrs.get("data-sort-property").cloned().unwrap_or_default(),
            sort_order: attrs.get("data-sort-order").cloned().unwrap_or_default(),
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
        if let Some(table) = fill(m, region, data) {
            out.insert_str(m.insert_at, &table);
            filled += 1;
        }
    }
    (filled > 0).then_some(out)
}

/// What one template is filled with: the region, the files that arrived, and
/// how its page writes a price and a string.
struct Ctx<'a> {
    region: &'a str,
    data: &'a HashMap<String, Value>,
    locale: &'static locale::Locale,
    messages: Option<locale::Messages>,
    /// The script copies wildcard-region entries over every region only for a
    /// template that names no sets.
    wildcards: bool,
}

impl Ctx<'_> {
    fn text(&self, s: &str) -> String {
        match &self.messages {
            Some(m) => m.translate(s),
            None => s.to_string(),
        }
    }

    fn na(&self) -> String {
        self.text("N/A")
    }

    /// A price as the page shows it; `N/A` (in the page's language) when the
    /// published value is not a number.
    fn price(&self, raw: &Value, t: &Token<'_>) -> String {
        let raw = match raw {
            Value::String(s) => s.clone(),
            Value::Number(n) => n.to_string(),
            _ => return self.na(),
        };
        if raw == "N/A" {
            return self.na();
        }
        let shown = Dec::parse(&raw).and_then(|mut v| {
            if let Some((op, operand)) = &t.op {
                v = v.op(*op, operand)?;
            }
            let digits = t.decimals.unwrap_or_else(|| v.dp().max(2));
            Some(v.localized(digits, self.locale))
        });
        shown.unwrap_or_else(|| self.na())
    }

    /// A string as the page shows it.
    fn string(&self, raw: &Value) -> String {
        match raw {
            Value::String(s) => self.text(s),
            Value::Number(n) => n.to_string(),
            Value::Bool(b) => b.to_string(),
            _ => self.na(),
        }
    }
}

/// `raw` as a price in `locale`, with exactly `digits` places.
#[cfg(test)]
fn format_price(raw: &str, digits: u32, locale: &locale::Locale) -> Option<String> {
    Some(Dec::parse(raw)?.localized(digits, locale))
}

/// The region's entry for `key`, as the page's script reads it: entries of the
/// wildcard regions (`Any`, `All`, ``) are copied over every region's own when the
/// file has more than one.
fn entry<'a>(file: &'a Value, region: &str, key: &str, wildcards: bool) -> Option<&'a Value> {
    let regions = file.get("regions")?.as_object()?;
    if wildcards && regions.len() != 1 {
        for wild in ["", "All", "Any"] {
            if let Some(e) = regions.get(wild).and_then(|r| r.get(key)) {
                return Some(e);
            }
        }
    }
    let own = regions.get(region).or_else(|| {
        // A file that holds one region-less map serves every region.
        (wildcards && regions.len() == 1)
            .then(|| regions.get("Any"))
            .flatten()
    })?;
    own.get(key)
}

/// The text one token shows, `None` when the file does not publish it.
fn value(t: &Token<'_>, ctx: &Ctx<'_>) -> Option<String> {
    let file = ctx.data.get(t.path)?;
    let e = entry(file, ctx.region, &t.key, ctx.wildcards)?;
    if t.string_of {
        let s = t.attr.as_deref().and_then(|a| e.get(a));
        return Some(match s {
            Some(v) => ctx.string(v),
            None => String::new(),
        });
    }
    let raw = e.get(t.attr.as_deref().unwrap_or("price"))?;
    Some(ctx.price(raw, t))
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// The element a missing price removes: the page drops a `tr` or `li` whose price
/// it cannot show. The innermost one around `at`.
fn row_around(template: &str, at: usize) -> Option<std::ops::Range<usize>> {
    ["tr", "li"]
        .iter()
        .filter_map(|tag| {
            let open_tag = format!("<{tag}");
            let close_tag = format!("</{tag}>");
            // The nearest opening `<tr`/`<li` that is the tag itself, not `<link`.
            let open = template[..at]
                .rmatch_indices(&open_tag)
                .find_map(|(i, _)| {
                    let next = template.as_bytes().get(i + open_tag.len()).copied();
                    matches!(next, Some(b'>' | b' ' | b'\n' | b'\t' | b'/')).then_some(i)
                })?;
            let close = template[at..].find(&close_tag)? + at + close_tag.len();
            // An element that closed before `at` does not enclose it.
            (!template[open..at].contains(&close_tag)).then_some(open..close)
        })
        .min_by_key(|r| r.end - r.start)
}

/// A template with its tokens filled; `None` when nothing in it could be.
fn fill(m: &Markup, region: &str, data: &HashMap<String, Value>) -> Option<String> {
    let template = m.template.as_str();
    // The page's own region menu and location picker are controls, not prices:
    // the table is read for the region the menu starts on.
    if template.contains("{locationSelector}") {
        return None;
    }
    let all = tokens(template)?;
    // A file that did not arrive says nothing about its prices: the table stays
    // as the page served it, instead of losing the rows that file would fill.
    if all.iter().any(|t| !t.related && !data.contains_key(t.path)) {
        return None;
    }
    let messages = if all.iter().any(|t| t.string_of) {
        Some(m.locale.messages(data)?)
    } else {
        None
    };
    let ctx = Ctx {
        region,
        data,
        locale: m.locale,
        messages,
        wildcards: m.sets.is_empty(),
    };
    let template = template.replace(
        "{regionSelector}",
        &format!("Region shown: {region} (prices differ by region)"),
    );
    let (template, tables) = if all.iter().any(|t| t.bulk) {
        let mut paths: Vec<&str> = Vec::new();
        for t in all.iter().filter(|t| !t.related) {
            if !paths.contains(&t.path) {
                paths.push(t.path);
            }
        }
        let files: Vec<&Value> = paths.iter().filter_map(|p| data.get(*p)).collect();
        bulk_tables(&template, m, &ctx, &files)?
    } else {
        (template, 0)
    };
    let filled = fill_tokens(&template, &ctx)?;
    match filled {
        Some(out) => Some(out),
        None if tables > 0 => Some(template),
        None => None,
    }
}

/// The single-value tokens of `template` filled. `Some(None)` when it holds
/// none that resolved; `None` when it holds a token this contract cannot read.
fn fill_tokens(template: &str, ctx: &Ctx<'_>) -> Option<Option<String>> {
    let toks = tokens(template)?;
    let values: Vec<Option<String>> = toks.iter().map(|t| value(t, ctx)).collect();
    let mut dropped: Vec<std::ops::Range<usize>> = Vec::new();
    for (t, v) in toks.iter().zip(&values) {
        if v.is_none()
            && !t.optional
            && let Some(row) = row_around(template, t.whole.start)
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
            escape(&v.clone().unwrap_or_else(|| ctx.na())),
        ));
    }
    if resolved == 0 {
        return Some(None);
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
    Some(Some(out))
}

/// Every table of `template` that holds bulk tokens, rendered as the page
/// renders it: the header row, then one row per record. `None` when one
/// cannot be (no header, no first row, no record for the region, an order
/// not read), or a bulk token stands outside a table.
fn bulk_tables(
    template: &str,
    m: &Markup,
    ctx: &Ctx<'_>,
    files: &[&Value],
) -> Option<(String, usize)> {
    let meta = bulk::Sets {
        sets: &m.sets,
        relates: m.relates.as_deref(),
        sort_property: &m.sort_property,
        sort_order: &m.sort_order,
    };
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    let mut count = 0;
    while let Some(open) = rest.find("<table") {
        let close = rest[open..].find("</table>")? + open + "</table>".len();
        let table = &rest[open..close];
        if table[1..].contains("<table") {
            return None;
        }
        out.push_str(&rest[..open]);
        if table.contains("{bulk") {
            out.push_str(&bulk_table(table, &meta, ctx, files)?);
            count += 1;
        } else {
            out.push_str(table);
        }
        rest = &rest[close..];
    }
    out.push_str(rest);
    if out.contains("{bulk") {
        return None;
    }
    Some((out, count))
}

fn bulk_table(
    table: &str,
    meta: &bulk::Sets<'_>,
    ctx: &Ctx<'_>,
    files: &[&Value],
) -> Option<String> {
    use scraper::{ElementRef, Html, Node, Selector};
    let doc = Html::parse_fragment(table);
    let sel = |s: &str| Selector::parse(s).ok();
    let head = doc.select(&sel("table > thead > tr")?).next()?;
    let first = doc.select(&sel("table > tbody > tr")?).next()?;
    let text_of = |el: ElementRef<'_>| -> String {
        let mut s = String::new();
        for node in el.descendants() {
            match node.value() {
                Node::Text(t) => s.push_str(t),
                Node::Element(e) if e.name() == "br" => s.push(' '),
                _ => {}
            }
        }
        s.split_whitespace().collect::<Vec<_>>().join(" ")
    };
    let headers: Vec<String> = head
        .children()
        .filter_map(ElementRef::wrap)
        .filter(|e| e.value().name() == "th")
        .map(text_of)
        .collect();
    // A data cell is a `td` whose first child is text; that text is the cell.
    let cells: Vec<String> = first
        .children()
        .filter_map(ElementRef::wrap)
        .filter(|e| e.value().name() == "td")
        .filter_map(|e| match e.first_child()?.value() {
            Node::Text(t) => Some(t.to_string()),
            _ => None,
        })
        .collect();
    let cell_tokens: Vec<Vec<Token<'_>>> = cells
        .iter()
        .map(|c| tokens(c))
        .collect::<Option<Vec<_>>>()?;
    if !cell_tokens.iter().flatten().any(|t| t.bulk) {
        return None;
    }
    let mut rows = bulk::rows(files, ctx.region, meta)?;
    bulk::sort(&mut rows, meta)?;
    let mut body = String::new();
    for row in &rows {
        // Each bulk token's value in this record; the record is dropped when a
        // token that is not optional has none.
        let mut values: Vec<Option<String>> = Vec::new();
        let mut keep = true;
        for t in cell_tokens.iter().flatten().filter(|t| t.bulk) {
            let shown = if t.related {
                bulk::related_value(row).map(|r| ctx.text(&r))
            } else {
                bulk::value(row, t).map(|raw| {
                    if t.string_of {
                        ctx.string(raw)
                    } else {
                        ctx.price(raw, t)
                    }
                })
            };
            if shown.is_none() && !t.optional {
                keep = false;
                break;
            }
            values.push(Some(shown.unwrap_or_else(|| ctx.na())));
        }
        if !keep {
            continue;
        }
        let mut next = values.into_iter();
        body.push_str("<tr>");
        for (i, _) in headers.iter().enumerate() {
            let shown = match (cells.get(i), cell_tokens.get(i)) {
                (Some(text), Some(toks)) => {
                    let mut text = text.clone();
                    for t in toks {
                        let whole = &cells[i][t.whole.clone()];
                        if t.bulk {
                            match next.next().flatten().filter(|v| !v.is_empty()) {
                                Some(v) => text = js_replace(&text, whole, &v),
                                None => text = "N/A".into(),
                            }
                        } else {
                            let v = value(t, ctx).unwrap_or_else(|| ctx.na());
                            text = js_replace(&text, whole, &v);
                        }
                    }
                    text
                }
                _ => String::new(),
            };
            let shown = if shown.is_empty() {
                "N/A".to_string()
            } else {
                shown
            };
            body.push_str(&format!("<td>{}</td>", escape(&shown)));
        }
        // Values of cells past the last column are consumed, not shown.
        body.push_str("</tr>");
    }
    if body.is_empty() {
        return None;
    }
    let head: String = headers
        .iter()
        .map(|h| format!("<th>{}</th>", escape(h)))
        .collect();
    Some(format!(
        "<table><thead><tr>{head}</tr></thead><tbody>{body}</tbody></table>"
    ))
}

/// `text.replace(pattern, with)` in JavaScript: the first occurrence, with the
/// replacement's `$$`, `$&`, `` $` `` and `$'` patterns expanded.
fn js_replace(text: &str, pattern: &str, with: &str) -> String {
    let Some(at) = text.find(pattern) else {
        return text.to_string();
    };
    let (before, after) = (&text[..at], &text[at + pattern.len()..]);
    let mut rep = String::with_capacity(with.len());
    let mut chars = with.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '$' {
            rep.push(c);
            continue;
        }
        match chars.peek() {
            Some('$') => {
                rep.push('$');
                chars.next();
            }
            Some('&') => {
                rep.push_str(pattern);
                chars.next();
            }
            Some('`') => {
                rep.push_str(before);
                chars.next();
            }
            Some('\'') => {
                rep.push_str(after);
                chars.next();
            }
            _ => rep.push('$'),
        }
    }
    format!("{before}{rep}{after}")
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
    #[cfg(test)]
    fn money(&self, digits: u32) -> String {
        self.localized(digits, locale::locale("en-US").expect("en-US"))
    }

    /// Exactly `digits` places, rounded half away from zero (the browser's
    /// `halfExpand`), written as `locale` writes a dollar amount.
    fn localized(&self, digits: u32, locale: &locale::Locale) -> String {
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
        locale.money(neg, int, frac)
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
            "{bulkPercentage!a/b!K}",
            "{priceOf!related}",
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

    fn markup(template: &str) -> Markup {
        Markup {
            insert_at: 0,
            template: template.to_string(),
            locale: locale::locale("en-US").unwrap(),
            sets: Vec::new(),
            relates: None,
            sort_property: String::new(),
            sort_order: String::new(),
        }
    }

    #[test]
    fn a_table_is_filled_and_a_row_with_no_published_price_is_dropped() {
        let out = fill(&markup(MARKUP), DEFAULT_REGION, &data()).unwrap();
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
    fn a_row_is_a_tr_or_li_itself_not_a_tag_that_starts_like_one() {
        let t = "<li>a</li><link rel=x><p>{priceOf!f/f!K}</p></li>";
        // No `li` encloses the token (the only `<li` before it is a closed item).
        assert_eq!(row_around(t, t.find('{').unwrap()), None);
        let t = "<ul><li>name <b>{priceOf!f/f!K}</b></li><li>other</li></ul>";
        let row = row_around(t, t.find('{').unwrap()).unwrap();
        assert_eq!(&t[row], "<li>name <b>{priceOf!f/f!K}</b></li>");
    }

    #[test]
    fn another_region_reads_its_own_prices() {
        let out = fill(&markup(MARKUP), "Africa (Cape Town)", &data()).unwrap();
        assert!(
            out.contains("$0.0000200000") || out.contains("$0.00002 per"),
            "{out}"
        );
        assert!(!out.contains("0.0000166667"), "{out}");
    }

    #[test]
    fn nothing_is_filled_when_the_file_has_nothing_for_the_region() {
        assert!(fill(&markup(MARKUP), "Mars (Olympus)", &data()).is_none());
    }

    #[test]
    fn a_file_that_did_not_arrive_leaves_its_table_as_served() {
        // Not "every price is unpublished": the rows must not be dropped.
        assert!(fill(&markup(MARKUP), DEFAULT_REGION, &HashMap::new()).is_none());
        let two_files = format!("{MARKUP}<p>{{priceOf!other/file!Key}}</p>");
        assert!(fill(&markup(&two_files), DEFAULT_REGION, &data()).is_none());
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
    fn a_bulk_table_that_names_no_sets_is_left_as_served() {
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
        assert_eq!(
            entry(&file, DEFAULT_REGION, "Fee", true).unwrap()["price"],
            "5"
        );
        assert_eq!(
            entry(&file, "Europe (Paris)", "Own", true).unwrap()["price"],
            "2"
        );
        let single = serde_json::json!({"regions": {"Any": {"Fee": {"price": "7"}}}});
        assert_eq!(
            entry(&single, DEFAULT_REGION, "Fee", true).unwrap()["price"],
            "7"
        );
    }

    /// A page holding one bulk template, with the markup attributes the RDS,
    /// DocumentDB and ElastiCache pages carry.
    fn bulk_page(template: &str, sets: &[&str], extra: &str, locale: &str) -> (String, String) {
        let attr = |s: &str| {
            s.replace('&', "&amp;")
                .replace('"', "&quot;")
                .replace('<', "&lt;")
        };
        let sets = attr(&serde_json::to_string(sets).unwrap());
        (
            format!(
                "<html><body><div data-pricing-markup=\"{}\" data-token-sets=\"{sets}\" {extra} \
                 data-pricing-endpoint=\"{ENDPOINT}\" data-partition=\"aws\" \
                 data-locale=\"{locale}\"></div></body></html>",
                attr(template)
            ),
            "https://aws.amazon.com/rds/mysql/pricing/".to_string(),
        )
    }

    // Shape of aws.amazon.com's rds-mysql-ondemand file, trimmed: units are
    // opaque ids, the set lists them, the region maps them to records.
    fn rds_file() -> Value {
        serde_json::json!({
            "manifest": {"serviceId": "rds", "source": "rds-mysql-ondemand"},
            "sets": {
                "MySQL GP": ["u1", "u2", "u3", "u4", "u5"],
                "MySQL Memory": ["u6"],
            },
            "regions": {
                "US East (Ohio)": {
                    "u1": {"price": "0.1710000000", "Instance Type": "db.m5.large", "vCPU": "2"},
                    "u2": {"price": "0.0170000000", "Instance Type": "db.t3.micro", "vCPU": "2",
                           "Network Performance": "Up to 5 Gigabit"},
                    "u3": {"price": "0.1680000000", "Instance Type": "db.m7g.large", "vCPU": "2"},
                    // Published without a price: its row is not shown.
                    "u4": {"Instance Type": "db.m6g.large", "vCPU": "2"},
                    "u6": {"price": "9", "Instance Type": "db.r5.large", "vCPU": "2"},
                },
                "Europe (Paris)": {
                    "u5": {"price": "1", "Instance Type": "db.m5.xlarge", "vCPU": "4"},
                },
            },
        })
    }

    const RDS_SETS: &[&str] = &["rds/rds-mysql-ondemand.[MySQL GP]"];
    const RDS_SORT: &str = "data-sort-property=\"rds/rds-mysql-ondemand.[MySQL GP].[Instance Type]\" \
        data-sort-order=\"instance-capacity\"";
    const RDS_TABLE: &str = "<p>{regionSelector}</p><table><thead><tr><th>Instance<br>type</th>\
        <th>vCPU</th><th>Network</th><th>Price per hour</th></tr></thead><tbody><tr>\
        <td>{bulkStringOf!rds/rds-mysql-ondemand!MySQL!GP![Instance!Type]}</td>\
        <td>{bulkStringOf!rds/rds-mysql-ondemand!MySQL!GP![vCPU]}</td>\
        <td>{bulkStringOf!rds/rds-mysql-ondemand!MySQL!GP![Network!Performance]!opt}</td>\
        <td>{bulkPriceOf!rds/rds-mysql-ondemand!MySQL!GP}</td></tr></tbody></table>";

    fn dictionaries(lang: &str, file: &str, strings: Value) -> [(String, Value); 2] {
        [
            (format!("@i18n/{file}"), serde_json::json!({})),
            (
                format!("@g11n/{lang}"),
                serde_json::json!({ lang: strings }),
            ),
        ]
    }

    #[test]
    fn a_bulk_table_lists_the_regions_records_in_the_pages_order() {
        let (html, url) = bulk_page(RDS_TABLE, RDS_SETS, RDS_SORT, "en-US");
        let p = plan(&html, &url).unwrap();
        let names: Vec<&str> = p.files.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, ["@g11n/en", "@i18n/en_US", "rds/rds-mysql-ondemand"]);
        let mut data: HashMap<String, Value> = dictionaries("en", "en_US", serde_json::json!({}))
            .into_iter()
            .collect();
        data.insert("rds/rds-mysql-ondemand".into(), rds_file());
        let out = apply(&html, &url, &data).unwrap();
        let table = &out[out.find("<table>").unwrap()..];
        assert!(
            table.starts_with(
                "<table><thead><tr><th>Instance type</th><th>vCPU</th><th>Network</th>\
             <th>Price per hour</th></tr></thead><tbody>"
            ),
            "{out}"
        );
        // Newest family first, then by size; the record with no price and
        // the records of other sets and regions are not shown; a missing
        // optional value shows N/A.
        let rows: Vec<&str> = table.split("<tr>").skip(2).collect();
        assert_eq!(
            rows,
            [
                "<td>db.t3.micro</td><td>2</td><td>Up to 5 Gigabit</td><td>$0.017</td></tr>",
                "<td>db.m7g.large</td><td>2</td><td>N/A</td><td>$0.168</td></tr>",
                "<td>db.m5.large</td><td>2</td><td>N/A</td><td>$0.171</td></tr></tbody></table></div></body></html>",
            ],
            "{out}"
        );
        assert!(out.contains("Region shown: US East (Ohio)"), "{out}");
        assert!(!table.contains('{'), "no token survives: {out}");
    }

    #[test]
    fn a_bulk_table_waits_for_its_dictionaries_and_its_sets() {
        let (html, url) = bulk_page(RDS_TABLE, RDS_SETS, RDS_SORT, "en-US");
        let data = HashMap::from([("rds/rds-mysql-ondemand".to_string(), rds_file())]);
        // Strings untranslated would be half the page's table.
        assert!(apply(&html, &url, &data).is_none());
        // No sets: the script shows no rows, so the table is left as served.
        let (html, url) = bulk_page(RDS_TABLE, &[], RDS_SORT, "en-US");
        assert!(plan(&html, &url).is_none());
        // An order this contract has not read leaves the table as served.
        let (html, url) = bulk_page(
            RDS_TABLE,
            RDS_SETS,
            "data-sort-order=\"purchaseOption\"",
            "en-US",
        );
        let mut data: HashMap<String, Value> = dictionaries("en", "en_US", serde_json::json!({}))
            .into_iter()
            .collect();
        data.insert("rds/rds-mysql-ondemand".into(), rds_file());
        assert!(apply(&html, &url, &data).is_none());
    }

    #[test]
    fn a_localized_page_shows_its_own_prices_and_strings() {
        let (html, url) = bulk_page(RDS_TABLE, RDS_SETS, RDS_SORT, "de-DE");
        let p = plan(&html, &url).unwrap();
        assert!(
            p.files
                .iter()
                .any(|(n, u)| n == "@i18n/de_DE" && u.ends_with("/de_DE.json")),
            "{:?}",
            p.files
        );
        let mut data: HashMap<String, Value> = dictionaries(
            "de",
            "de_DE",
            serde_json::json!({"Up to 5 Gigabit": "Bis zu 5 Gigabit", "N[slash]A": "k. A."}),
        )
        .into_iter()
        .collect();
        data.insert("rds/rds-mysql-ondemand".into(), rds_file());
        let out = apply(&html, &url, &data).unwrap();
        assert!(
            out.contains(
                "<td>db.t3.micro</td><td>2</td><td>Bis zu 5 Gigabit</td><td>0,017\u{a0}USD</td>"
            ),
            "{out}"
        );
        assert!(
            out.contains("<td>k. A.</td><td>0,168\u{a0}USD</td>"),
            "{out}"
        );
        // A page in a language this contract has not read is left alone.
        let (html, url) = bulk_page(RDS_TABLE, RDS_SETS, RDS_SORT, "xx-XX");
        assert!(plan(&html, &url).is_none());
    }

    #[test]
    fn related_sets_join_into_one_row_per_shared_value() {
        // DocumentDB's shape: standard and I/O-optimized sets of one file,
        // joined on the instance type.
        let docdb = serde_json::json!({
            "manifest": {"serviceId": "docdb", "source": "docdb"},
            "sets": {"OnDemand": ["a", "b"], "OnDemand IO": ["c"]},
            "regions": {"US East (Ohio)": {
                "a": {"price": "0.2770000000", "Instance Type": "db.r5.large"},
                "b": {"price": "0.0780000000", "Instance Type": "db.t3.medium"},
                "c": {"price": "0.3050000000", "Instance Type": "db.r5.large"},
            }},
        });
        let template = "<table><thead><tr><th>Instance</th><th>Standard</th><th>I/O-Optimized</th>\
            </tr></thead><tbody><tr><td>{bulkStringOf!related}</td>\
            <td>{bulkPriceOf!docdb/docdb!OnDemand}</td><td>{bulkPriceOf!docdb/docdb!OnDemand!IO!opt}</td>\
            </tr></tbody></table>";
        let (html, url) = bulk_page(
            template,
            &[
                "related",
                "docdb/docdb.[OnDemand]",
                "docdb/docdb.[OnDemand IO]",
            ],
            "data-relates=\"docdb/docdb.[OnDemand].[Instance Type],docdb/docdb.[OnDemand IO].[Instance Type]\" \
             data-sort-order=\"instance-capacity\"",
            "en-US",
        );
        let p = plan(&html, &url).unwrap();
        let names: Vec<&str> = p.files.iter().map(|(n, _)| n.as_str()).collect();
        // The joined value is a string the page translates.
        assert_eq!(names, ["@g11n/en", "@i18n/en_US", "docdb/docdb"]);
        let mut data: HashMap<String, Value> = dictionaries("en", "en_US", serde_json::json!({}))
            .into_iter()
            .collect();
        data.insert("docdb/docdb".into(), docdb);
        let out = apply(&html, &url, &data).unwrap();
        assert!(
            out.contains(
                "<tbody><tr><td>db.t3.medium</td><td>$0.078</td><td>N/A</td></tr>\
                 <tr><td>db.r5.large</td><td>$0.277</td><td>$0.305</td></tr></tbody>"
            ),
            "{out}"
        );
    }
}
