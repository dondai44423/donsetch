//! The EC2 on-demand grid: the page's own app, asked for the table it shows.
//!
//! `aws.amazon.com/ec2/pricing/on-demand/` carries an iframe whose app
//! (`c0.b0.p.awsstatic.com/#/ec2/on-demand-plan`) is configured at run time: its
//! region, operating system and instance filters are UI state, and the table it
//! shows — instance name and On-Demand hourly rate, 1,228 rows for its default
//! region — is built in the browser from one price file. The page's own pricing
//! templates do not carry that table, so a reader that fills only the templates
//! shows no instance price at all.
//!
//! This module reads the file the app reads, for the app's own default
//! selection (Linux, the page's default region, all instance types), and puts
//! the rows where the iframe sits. The file is AWS's published price data at its
//! own pricing host; nothing is rendered, and the table says which selection and
//! publication it describes.

use serde_json::Value;

/// The app's data file: AWS's price-publish host, exact path. The app's bundle
/// names this URL; a different host or path is not the contract.
const GRID_HOST: &str = "view-publish.us-west-2.prod.pricing.aws.a2z.com";
const GRID_PATH: &str = "/pricing/2.0/meteredUnitMaps/ec2/USD/current/ec2.json";
/// The instance keys the file carries for the app's default OS.
const LINUX: &str = "OnDemand Linux-instancetype-";

/// One grid to fill: where the iframe sits, and the file and selection to read.
pub(super) struct Grid {
    /// Byte offset just past the iframe's opening tag.
    pub insert_at: usize,
    pub url: String,
    /// The region the app shows when the visitor has chosen none.
    pub region: &'static str,
}

/// The grid this page carries, when it is the EC2 on-demand page and the
/// iframe's app still reads the file this contract was read from.
pub(super) fn plan(html: &str, page: &url::Url) -> Option<Grid> {
    if page.scheme() != "https" || page.host_str() != Some(super::PAGE_HOST) {
        return None;
    }
    if !page.path().contains("/ec2/pricing") {
        return None;
    }
    let at = html.find(r#"data-test="ec2/on-demand-plan""#)?;
    let start = html[..at].rfind('<')?;
    let end = super::tag_end(html, start)?;
    // Read `data-src` straight from the tag: the URL is the whole fact needed
    // here, and a plain scan cannot disagree with the HTML parser about an
    // attribute the browser itself reads as text.
    let tag = &html[start..=end];
    let src = tag
        .split_once("data-src=")
        .and_then(|(_, rest)| {
            let rest = rest.trim_start();
            let quote = rest.chars().next()?;
            if quote != '"' && quote != '\'' {
                return None;
            }
            let value = &rest[1..];
            let close = value.find(quote)?;
            Some(&value[..close])
        })?;
    // The app's own address names the plan; a page that points elsewhere is a
    // different widget and is left as served.
    if !src.contains("c0.b0.p.awsstatic.com/#/ec2/on-demand-plan") {
        return None;
    }
    // The table belongs beside the widget, not inside it: an iframe's content
    // is fallback text to an HTML parser, so a table placed between the tags
    // would never become one.
    let rest = &html[end + 1..];
    let insert_at = match rest.to_ascii_lowercase().find("</iframe>") {
        Some(close) if close < 256 => end + 1 + close + "</iframe>".len(),
        _ => end + 1,
    };
    Some(Grid {
        insert_at,
        url: format!("https://{GRID_HOST}{GRID_PATH}"),
        region: super::DEFAULT_REGION,
    })
}

/// The rows the app's default selection shows, as a table placed where the
/// iframe is. `None` when the file is not the shape this contract was read
/// from, so a changed file leaves the page as served instead of inventing rows.
pub(super) fn render(json: &Value, region: &str) -> Option<String> {
    let rows = json.get("regions")?.get(region)?.as_object()?;
    let mut items: Vec<(&str, &str)> = Vec::new();
    for (key, record) in rows {
        let Some(instance) = key.strip_prefix(LINUX) else {
            continue;
        };
        let Some(price) = record.get("price").and_then(Value::as_str) else {
            continue;
        };
        let price = price.trim();
        if instance.is_empty() || price.is_empty() || !price.bytes().all(|b| b.is_ascii_digit() || b == b'.') {
            continue;
        }
        items.push((instance, price));
    }
    if items.is_empty() {
        return None;
    }
    items.sort_by(|a, b| a.0.cmp(b.0));
    let published = json
        .get("manifest")
        .and_then(|m| m.get("ETLIngestionTriggerDate"))
        .and_then(Value::as_str)
        .and_then(|d| d.split('T').next())
        .unwrap_or_default();
    let mut table = String::with_capacity(items.len() * 96 + 256);
    table.push_str("<table><caption>Amazon EC2 On-Demand pricing — Linux, ");
    table.push_str(region);
    table.push_str(", USD per hour");
    if !published.is_empty() {
        table.push_str(" (published ");
        table.push_str(published);
        table.push(')');
    }
    table.push_str("</caption><thead><tr><th>Instance name</th><th>On-Demand hourly rate</th></tr></thead><tbody>");
    for (instance, price) in items {
        table.push_str("<tr><td>");
        table.push_str(instance);
        table.push_str("</td><td>$");
        table.push_str(trim_zeros(price));
        table.push_str("</td></tr>");
    }
    table.push_str("</tbody></table>");
    Some(table)
}

/// `0.0416000000` as the page would print a price without its padded zeros.
fn trim_zeros(price: &str) -> &str {
    if !price.contains('.') {
        return price;
    }
    let trimmed = price.trim_end_matches('0');
    trimmed.strip_suffix('.').unwrap_or(trimmed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page(path: &str) -> url::Url {
        url::Url::parse(&format!("https://aws.amazon.com{path}")).unwrap()
    }

    fn price_file() -> Value {
        serde_json::json!({
            "manifest": {"ETLIngestionTriggerDate": "2026-11-07T00:28:52Z"},
            "regions": {
                "US East (Ohio)": {
                    "OnDemand Linux-instancetype-t3.medium": {"price": "0.0416000000"},
                    "OnDemand Linux-instancetype-m7g.large": {"price": "0.0816000000"},
                    "hashed-key": {"price": "9.99"},
                    "OnDemand Windows-instancetype-t3.medium": {"price": "0.0832"}
                },
                "US West (Oregon)": {
                    "OnDemand Linux-instancetype-t3.medium": {"price": "0.0416"}
                }
            }
        })
    }

    #[test]
    fn the_grid_is_planned_only_on_the_ec2_page_with_its_own_app() {
        let html = r#"<p>x</p><iframe sandbox="allow-scripts" data-src="https://c0.b0.p.awsstatic.com/#/ec2/on-demand-plan" data-test="ec2/on-demand-plan" loading="lazy"></iframe>"#;
        let planned = plan(html, &page("/ec2/pricing/on-demand/")).expect("grid");
        assert_eq!(planned.region, "US East (Ohio)");
        assert_eq!(planned.insert_at, html.find("</iframe>").unwrap() + "</iframe>".len());
        assert!(plan(html, &page("/s3/pricing/")).is_none());
        let other = html.replace("c0.b0.p.awsstatic.com", "example.com");
        assert!(plan(&other, &page("/ec2/pricing/on-demand/")).is_none());
    }

    #[test]
    fn the_default_selection_renders_its_prices_sorted_and_trimmed() {
        let table = render(&price_file(), "US East (Ohio)").expect("table");
        assert!(table.contains("<th>Instance name</th>"), "{table}");
        assert!(table.contains("Linux, US East (Ohio), USD per hour"), "{table}");
        assert!(table.contains("published 2026-11-07"), "{table}");
        // Linux rows only, hashed and Windows entries left out, sorted by name.
        assert!(table.contains("<td>m7g.large</td><td>$0.0816</td>"), "{table}");
        assert!(table.contains("<td>t3.medium</td><td>$0.0416</td>"), "{table}");
        assert!(!table.contains("hashed-key"), "{table}");
        assert!(!table.contains("9.99"), "{table}");
        assert!(!table.contains("Windows"), "{table}");
        assert!(
            table.find("m7g.large").unwrap() < table.find("t3.medium").unwrap(),
            "{table}"
        );
    }

    #[test]
    fn a_file_without_the_selection_renders_nothing() {
        assert!(render(&price_file(), "Mars (Olympus)").is_none());
        assert!(render(&serde_json::json!({"regions": {}}), "US East (Ohio)").is_none());
        assert!(render(&serde_json::json!({}), "US East (Ohio)").is_none());
    }

    #[test]
    fn prices_lose_only_their_padding() {
        assert_eq!(trim_zeros("0.0416000000"), "0.0416");
        assert_eq!(trim_zeros("3"), "3");
        assert_eq!(trim_zeros("1.000"), "1");
    }
}
