//! The crawl digest: the evidence document an agent reads before it
//! spends a handle. Lossy in the brief, lossless through `crawl get pN`.

use crate::crawl::model::plan::Plan;
use crate::crawl::{CrawlPage, CrawlResult};
use serde::Serialize;
use std::collections::{HashMap, HashSet};

/// The digest is the evidence document, so its size follows the plan's
/// page count: a 10-page crawl spends its context on 10 real cards, not on
/// a fixed 900-character brief that drops what the caller asked to read.
/// The floor and ceiling keep a 200-page crawl inside one tool result.
const CARD_MIN_CHARS: usize = 500;
const CARD_MAX_CHARS: usize = 3_200;
const DIGEST_MIN_CHARS: usize = 14_000;
const DIGEST_MAX_CHARS: usize = 60_000;
const MAX_HEADINGS: usize = 32;

#[derive(Debug, Clone, Serialize)]
pub struct DigestPage {
    pub handle: String,
    pub title: String,
    pub url: String,
    pub section: String,
    pub template: String,
    pub kind: String,
    pub lastmod: Option<String>,
    pub headings: Vec<String>,
    pub card: String,
    pub chars: usize,
    pub full_chars: usize,
    pub focus: bool,
    pub duplicate_of: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Digest {
    pub host: String,
    pub seed: String,
    pub job: Option<String>,
    pub pages: Vec<DigestPage>,
    pub read_chars: usize,
    pub digest_chars: usize,
    pub boilerplate_removed: usize,
    pub collapsed: usize,
    pub focus: Option<String>,
    pub sections_covered: usize,
    pub sections_total: usize,
    pub families_covered: usize,
    pub families_total: usize,
    /// Pages this plan selected and the inventory's in-scope pages: the
    /// honest numerator/denominator of what the crawl covered.
    pub planned: usize,
    pub inventory_urls: usize,
    pub reserves: usize,
    pub url_weight: f64,
    pub focus_matches: usize,
    pub focus_precision: f64,
    /// The caller's topics, and the ones no planned page answered.
    pub topics_answered: usize,
    pub topics_total: usize,
    pub topics_unanswered: Vec<String>,
    pub stop: String,
    pub wall: Option<String>,
    pub text: String,
}

#[derive(Debug, Clone, Default)]
struct Block {
    text: String,
    norm: String,
    heading: bool,
    code: bool,
    #[allow(dead_code)]
    start: usize,
}

fn norm(s: &str) -> String {
    s.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

fn strip_frontmatter(markdown: &str) -> String {
    let mut s = markdown;
    if let Some(rest) = s.strip_prefix("---\n")
        && let Some(end) = rest.find("\n---\n")
    {
        s = &rest[end + 5..];
    }
    s.trim_start().to_string()
}

fn parse_blocks(markdown: &str) -> (Vec<String>, Vec<Block>) {
    let text = strip_frontmatter(markdown);
    let mut headings = Vec::new();
    let mut blocks = Vec::new();
    let mut cur = String::new();
    let mut cur_start = 0usize;
    let mut in_fence = false;
    let mut fence = String::new();
    let mut fence_start = 0usize;

    let flush = |cur: &mut String, start: usize, blocks: &mut Vec<Block>| {
        let t = cur.trim();
        if !t.is_empty() {
            blocks.push(Block {
                text: t.to_string(),
                norm: norm(t),
                heading: false,
                code: false,
                start,
            });
        }
        cur.clear();
    };

    for line in text.lines() {
        if in_fence {
            fence.push_str(line);
            fence.push('\n');
            if line.trim_start().starts_with("```") {
                let t = fence.trim_end().to_string();
                blocks.push(Block {
                    text: t.clone(),
                    norm: norm(&t),
                    heading: false,
                    code: true,
                    start: fence_start,
                });
                fence.clear();
                in_fence = false;
            }
            continue;
        }
        if line.trim_start().starts_with("```") {
            flush(&mut cur, cur_start, &mut blocks);
            in_fence = true;
            fence.clear();
            fence.push_str(line);
            fence.push('\n');
            fence_start = blocks.len() + headings.len();
            continue;
        }
        let t = line.trim();
        let is_heading = t.starts_with('#')
            && t.chars().take_while(|c| *c == '#').count().clamp(1, 6) > 0
            && t.chars().nth(t.chars().take_while(|c| *c == '#').count()) == Some(' ');
        if is_heading {
            flush(&mut cur, cur_start, &mut blocks);
            let title = t.trim_start_matches('#').trim().to_string();
            if !title.is_empty() {
                headings.push(title.clone());
                blocks.push(Block {
                    text: t.to_string(),
                    norm: norm(&title),
                    heading: true,
                    code: false,
                    start: blocks.len() + headings.len(),
                });
            }
            cur_start = blocks.len() + headings.len();
            continue;
        }
        if t.is_empty() {
            flush(&mut cur, cur_start, &mut blocks);
            cur_start = blocks.len() + headings.len();
            continue;
        }
        if cur.is_empty() {
            cur_start = blocks.len() + headings.len();
        }
        if !cur.is_empty() {
            cur.push('\n');
        }
        cur.push_str(line);
    }
    flush(&mut cur, cur_start, &mut blocks);
    if !fence.trim().is_empty() {
        let t = fence.trim_end().to_string();
        blocks.push(Block {
            text: t.clone(),
            norm: norm(&t),
            heading: false,
            code: true,
            start: fence_start,
        });
    }
    (headings, blocks)
}

fn plan_row<'a>(
    plan: Option<&'a Plan>,
    url: &str,
) -> Option<&'a crate::crawl::model::plan::PlanRow> {
    let plan = plan?;
    plan.pages
        .iter()
        .chain(plan.reserves.iter())
        .find(|r| r.url == url)
}

fn url_section(url: &str) -> String {
    url::Url::parse(url)
        .ok()
        .map(|u| {
            let mut parts = u.path_segments().into_iter().flatten();
            let _ = parts.next();
            parts.take(3).collect::<Vec<_>>().join("/")
        })
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "/".to_string())
}

fn page_sig(page: &CrawlPage) -> u64 {
    use std::hash::{Hash, Hasher};
    let (_, blocks) = parse_blocks(&page.markdown);
    let body = blocks
        .iter()
        .filter(|b| !b.heading)
        .map(|b| b.norm.as_str())
        .collect::<String>();
    let mut h = std::collections::hash_map::DefaultHasher::new();
    page.title.to_lowercase().hash(&mut h);
    body.chars().take(400).collect::<String>().hash(&mut h);
    h.finish()
}

/// Build the digest. Handles are `pN` by page position in the job, so they
/// stay stable when a resumed job appends more pages.
pub fn build(result: &CrawlResult) -> Digest {
    let plan = result.plan.as_ref();
    let focus = plan.and_then(|p| p.focus.clone());
    let kept = result.pages.iter().filter(|p| !p.duplicate).count().max(1);
    let digest_cap = (kept * 2_400).clamp(DIGEST_MIN_CHARS, DIGEST_MAX_CHARS);
    let card_cap = (digest_cap / kept).clamp(CARD_MIN_CHARS, CARD_MAX_CHARS);
    let focus_terms: Vec<String> = focus
        .as_deref()
        .unwrap_or("")
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| t.chars().count() > 2)
        .map(str::to_lowercase)
        .collect();

    // Parse every page before rendering so repeated prose can be removed
    // from every card, not only from the pages that happen to follow it.
    let parsed: Vec<(String, Vec<String>, Vec<Block>)> = result
        .pages
        .iter()
        .map(|p| {
            let (h, b) = parse_blocks(&p.markdown);
            (p.title.clone(), h, b)
        })
        .collect();
    let mut freq: HashMap<String, usize> = HashMap::new();
    for (_, _, blocks) in &parsed {
        for b in blocks.iter().filter(|b| !b.heading && !b.code) {
            if b.text.chars().count() >= 40 {
                *freq.entry(b.norm.clone()).or_default() += 1;
            }
        }
    }
    let boilerplate: HashSet<String> = freq
        .into_iter()
        .filter(|(_, n)| *n >= 2)
        .map(|(k, _)| k)
        .collect();

    let mut seen = HashMap::<u64, String>::new();
    let mut pages = Vec::new();
    let mut boilerplate_removed = 0usize;
    let mut collapsed = 0usize;
    let mut used = 0usize;
    for (index, page) in result.pages.iter().enumerate() {
        if page.duplicate {
            collapsed += 1;
            continue;
        }
        let sig = page_sig(page);
        let duplicate_of = seen.get(&sig).cloned();
        if duplicate_of.is_some() {
            collapsed += 1;
            continue;
        }
        seen.insert(sig, format!("p{}", index + 1));

        let (_, headings, blocks) = &parsed[index];
        let row = plan_row(plan, &page.url);
        let section = row
            .map(|r| r.section.clone())
            .unwrap_or_else(|| url_section(&page.url));
        let template = row.map(|r| r.template.clone()).unwrap_or_default();
        let mut candidates: Vec<(i64, usize, &Block)> = blocks
            .iter()
            .enumerate()
            .filter(|(_, b)| !b.heading)
            .filter(|(_, b)| {
                if b.code {
                    focus_terms.iter().any(|t| b.norm.contains(t)) || focus_terms.is_empty()
                } else if boilerplate.contains(&b.norm) {
                    boilerplate_removed += 1;
                    false
                } else {
                    true
                }
            })
            .map(|(i, b)| {
                let hits = focus_terms
                    .iter()
                    .filter(|t| b.norm.contains(t.as_str()))
                    .count() as i64;
                let len = b.text.chars().count() as i64;
                let mut score = hits * 100;
                score += if b.code {
                    if hits > 0 { 25 } else { 2 }
                } else {
                    (len.min(600) / 20).max(1)
                };
                (score, i, b)
            })
            .collect();
        candidates.sort_by_key(|(score, i, _)| (std::cmp::Reverse(*score), *i));

        let heading_text = headings
            .iter()
            .take(MAX_HEADINGS)
            .cloned()
            .collect::<Vec<_>>()
            .join(" · ");
        let mut header = format!(
            "{} · {} · {}\n",
            page.url,
            section,
            format!("{:?}", page.kind).to_lowercase()
        );
        if !heading_text.is_empty() {
            header.push_str(&format!("H: {heading_text}\n"));
        }
        let header_chars = header.chars().count();
        let allowed = card_cap.saturating_sub(header_chars).max(200);
        let mut chosen: Vec<usize> = Vec::new();
        let mut body = String::new();
        for (_, i, b) in candidates {
            let excerpt = truncate_chars(&b.text, allowed.saturating_sub(body.chars().count()));
            if excerpt.is_empty() {
                break;
            }
            if !body.is_empty() {
                body.push_str("\n\n");
            }
            body.push_str(&excerpt);
            chosen.push(i);
            if body.chars().count() >= allowed {
                break;
            }
        }
        let _ = chosen;
        let mut card = header;
        if body.is_empty() {
            card.push_str("\n*(headings only)*");
        } else {
            card.push('\n');
            card.push_str(&body);
        }
        card = truncate_chars(&card, card_cap);
        if used + card.chars().count() > digest_cap {
            card = truncate_chars(&card, digest_cap.saturating_sub(used));
            if card.chars().count() < 80 {
                break;
            }
        }
        used += card.chars().count();
        pages.push(DigestPage {
            handle: format!("p{}", index + 1),
            title: page.title.clone(),
            url: page.url.clone(),
            section,
            template,
            kind: format!("{:?}", page.kind).to_lowercase(),
            lastmod: page.lastmod.clone(),
            headings: headings.iter().take(MAX_HEADINGS).cloned().collect(),
            card,
            chars: 0,
            full_chars: page.markdown.chars().count(),
            focus: page.focus.is_some() || row.is_some_and(|r| r.focus),
            duplicate_of,
        });
        if let Some(last) = pages.last_mut() {
            last.chars = last.card.chars().count();
        }
    }

    // A second pass catches source headers that only become identical at
    // render time (the first pass sees the page-level blocks before they are
    // selected into cards).
    dedupe_cards(&mut pages, &mut boilerplate_removed);
    dedupe_lines(&mut pages, &mut boilerplate_removed);

    let read_chars = result.pages.iter().map(|p| p.chars).sum();
    let digest_chars = pages.iter().map(|p| p.chars).sum();
    let planned = plan.map_or(pages.len(), |p| p.pages.len());
    let host = result
        .model
        .as_ref()
        .map(|m| m.host.clone())
        .or_else(|| {
            url::Url::parse(&result.seed)
                .ok()
                .and_then(|u| u.host_str().map(str::to_string))
        })
        .unwrap_or_else(|| result.seed.clone());
    let mut d = Digest {
        host,
        seed: result.seed.clone(),
        job: result.run.as_ref().map(|r| r.job.clone()),
        pages,
        read_chars,
        digest_chars,
        boilerplate_removed,
        collapsed,
        focus,
        sections_covered: plan.map_or(0, |p| p.coverage.sections_covered),
        sections_total: plan.map_or(0, |p| p.coverage.sections_total),
        families_covered: plan.map_or(0, |p| p.coverage.families_covered),
        families_total: plan.map_or(0, |p| p.coverage.families_total),
        planned,
        inventory_urls: plan.map_or(0, |p| p.coverage.urls_total),
        reserves: plan.map_or(0, |p| p.reserves.len()),
        url_weight: plan.map_or(0.0, |p| p.coverage.url_weight),
        focus_matches: plan.map_or(0, |p| p.coverage.focus_matches),
        focus_precision: plan.map_or(0.0, |p| p.coverage.focus_precision),
        topics_answered: plan.map_or(0, |p| p.coverage.focus_topics_answered),
        topics_total: plan.map_or(0, |p| p.coverage.focus_topics),
        topics_unanswered: plan
            .map(|p| p.coverage.focus_unanswered.clone())
            .unwrap_or_default(),
        stop: result
            .run
            .as_ref()
            .map(|r| r.stop.as_str().to_string())
            .unwrap_or_else(|| format!("{:?}", result.stop)),
        wall: result.run.as_ref().and_then(|r| r.wall.as_ref()).map(|w| {
            format!(
                "{} · {}",
                w.class,
                w.vendor.as_deref().unwrap_or("unknown vendor")
            )
        }),
        text: String::new(),
    };
    d.text = render(&d);
    d
}

/// Remove every body block that repeats across cards. This is the last
/// gate against a source header or shared footer that survived the page-level
/// frequency pass. Card headings and the `H:` line are never touched.
fn dedupe_cards(pages: &mut [DigestPage], removed: &mut usize) {
    let mut freq: HashMap<String, usize> = HashMap::new();
    for p in pages.iter() {
        for block in card_body(&p.card).1 {
            let n = norm(&block);
            if n.chars().count() >= 40 {
                *freq.entry(n).or_default() += 1;
            }
        }
    }
    let repeated: HashSet<String> = freq
        .into_iter()
        .filter(|(_, n)| *n >= 2)
        .map(|(k, _)| k)
        .collect();
    if repeated.is_empty() {
        return;
    }
    for p in pages.iter_mut() {
        let (header, blocks) = card_body(&p.card);
        let before = blocks.len();
        let kept: Vec<String> = blocks
            .into_iter()
            .filter(|b| !repeated.contains(&norm(b)))
            .collect();
        *removed += before.saturating_sub(kept.len());
        p.card = if kept.is_empty() {
            format!("{header}\n\n*(headings only)*")
        } else {
            format!("{header}\n\n{}", kept.join("\n\n"))
        };
        p.chars = p.card.chars().count();
    }
}

/// A repeated source header can share a paragraph with a url/quote, so the
/// block pass sees a different paragraph on each page. Remove repeated long
/// lines too; headings and the `H:` line remain.
fn dedupe_lines(pages: &mut [DigestPage], removed: &mut usize) {
    let mut freq: HashMap<String, usize> = HashMap::new();
    for p in pages.iter() {
        let (_, blocks) = card_body(&p.card);
        for line in blocks.iter().flat_map(|b| b.lines()) {
            let n = norm(line);
            if n.chars().count() >= 40 {
                *freq.entry(n).or_default() += 1;
            }
        }
    }
    let repeated: HashSet<String> = freq
        .into_iter()
        .filter(|(_, n)| *n >= 2)
        .map(|(k, _)| k)
        .collect();
    if repeated.is_empty() {
        return;
    }
    for p in pages.iter_mut() {
        let (header, blocks) = card_body(&p.card);
        let mut kept: Vec<String> = Vec::new();
        let mut dropped = 0usize;
        for block in blocks {
            let lines: Vec<&str> = block
                .lines()
                .filter(|line| {
                    let n = norm(line);
                    if n.chars().count() >= 40 && repeated.contains(&n) {
                        dropped += 1;
                        false
                    } else {
                        true
                    }
                })
                .collect();
            let b = lines.join("\n").trim().to_string();
            if !b.is_empty() {
                kept.push(b);
            }
        }
        *removed += dropped;
        p.card = if kept.is_empty() {
            format!("{header}\n\n*(headings only)*")
        } else {
            format!("{header}\n\n{}", kept.join("\n\n"))
        };
        p.chars = p.card.chars().count();
    }
}

fn card_body(card: &str) -> (String, Vec<String>) {
    match card.split_once("\n\n") {
        Some((header, body)) => (
            header.to_string(),
            body.split("\n\n")
                .map(str::trim)
                .filter(|b| !b.is_empty())
                .map(str::to_string)
                .collect(),
        ),
        None => (card.to_string(), Vec::new()),
    }
}

fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max).collect();
    if let Some(i) = out.rfind(char::is_whitespace) {
        out.truncate(i);
    }
    out.push('…');
    out
}

/// Render the site brief, page cards, gaps and next actions.
pub fn render(d: &Digest) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "# {} — crawl digest\n{} pages · {} chars read · {} digest chars · stop {}\n",
        d.host,
        d.pages.len(),
        d.read_chars,
        d.digest_chars,
        d.stop
    ));
    if d.sections_total > 0 {
        out.push_str(&format!(
            "coverage: {}/{} sections ({}% of URLs) · {}/{} families",
            d.sections_covered,
            d.sections_total,
            (d.url_weight * 100.0).round() as usize,
            d.families_covered,
            d.families_total
        ));
        if d.inventory_urls > 0 {
            out.push_str(&format!(
                " · {} of {} inventory pages planned",
                d.planned, d.inventory_urls
            ));
        }
        out.push('\n');
    }
    if let Some(focus) = &d.focus {
        out.push_str(&format!(
            "focus \"{focus}\": {}/{} topics answered · {} of {} pages focus",
            d.topics_answered,
            d.topics_total,
            d.focus_matches,
            d.pages.len().max(1)
        ));
        out.push('\n');
        if !d.topics_unanswered.is_empty() {
            out.push_str(&format!(
                "focus gaps: {} — no inventory page carries the term\n",
                d.topics_unanswered.join(", ")
            ));
        }
    }
    if d.boilerplate_removed > 0 || d.collapsed > 0 {
        out.push_str(&format!(
            "dedupe: {} boilerplate block(s) removed, {} page(s) collapsed\n",
            d.boilerplate_removed, d.collapsed
        ));
    }
    if let Some(wall) = &d.wall {
        out.push_str(&format!("wall: declared · {wall}\n"));
    }
    out.push('\n');
    for p in &d.pages {
        out.push_str(&format!("## [{}] {}\n", p.handle, p.title));
        out.push_str(&p.card);
        out.push_str("\n\n");
    }
    if d.pages.is_empty() {
        out.push_str("## No pages\nNo readable page was returned.\n\n");
    }
    out.push_str("## Not fetched\n");
    if d.inventory_urls > d.planned {
        out.push_str(&format!(
            "{} of {} in-scope page(s) are outside this plan (budget or a family already represented).\n",
            d.inventory_urls - d.planned,
            d.inventory_urls
        ));
    }
    if d.families_total > d.families_covered {
        out.push_str(&format!(
            "{} family(ies) not represented.\n",
            d.families_total - d.families_covered
        ));
    }
    if d.reserves > 0 {
        out.push_str(&format!(
            "{} reserve page(s) stand by if a planned page fails.\n",
            d.reserves
        ));
    }
    out.push_str("next: ");
    if let Some(p) = d.pages.first() {
        match &d.job {
            Some(job) => out.push_str(&format!("`crawl get {} --job {job}`", p.handle)),
            None => out.push_str(&format!("`crawl get {}`", p.handle)),
        }
    } else {
        out.push_str("`crawl plan`");
    }
    if let Some(focus) = &d.focus {
        out.push_str(&format!(" · `crawl plan --focus \"{focus}\"`"));
    }
    out.push('\n');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crawl::{CrawlResult, DeniedLog, InventoryStatus, StopReason};
    use crate::extract::ContentKind;
    use std::time::Duration;

    fn page(url: &str, title: &str, body: &str, duplicate: bool) -> CrawlPage {
        CrawlPage {
            focus: None,
            url: url.into(),
            title: title.into(),
            kind: ContentKind::Page,
            markdown: body.into(),
            chars: body.chars().count(),
            next_offset: None,
            partial: None,
            quality: 0.8,
            duplicate,
            parent: None,
            score: 0.0,
            lastmod: None,
            fetched_at: 0,
            fingerprint: None,
        }
    }

    #[test]
    fn digest_keeps_headings_removes_repeated_blocks_and_collapses_duplicates() {
        let common = "This source header is repeated on every page and must not be boilerplate in the digest.";
        let p1 = page(
            "https://ex.com/a",
            "Page A",
            &format!(
                "# Page A\n\n## Install\n\n{common}\n\nUnique alpha content with enough words to survive the card's density ranking.\n"
            ),
            false,
        );
        let p2 = page(
            "https://ex.com/b",
            "Page B",
            &format!(
                "# Page B\n\n## Usage\n\n{common}\n\nUnique beta content with enough words to survive the card's density ranking.\n"
            ),
            false,
        );
        let p3 = page(
            "https://ex.com/c",
            "Page C",
            "# Page C\n\n## Duplicate\n\nSame body as another page.\n",
            true,
        );
        let result = CrawlResult {
            inventory: InventoryStatus::default(),
            seed: "https://ex.com/".into(),
            pages: vec![p1, p2, p3],
            queued: Vec::new(),
            depth_cut: Vec::new(),
            filtered_out: 0,
            skipped: Vec::new(),
            stop: StopReason::FrontierEmpty,
            elapsed: Duration::ZERO,
            map: Vec::new(),
            crawl_delay: None,
            resume: None,
            denied: DeniedLog::default(),
            seed_failure: None,
            model: None,
            plan: None,
            run: None,
            inventory_urls: Vec::new(),
            delta: None,
        };
        let d = build(&result);
        assert_eq!(d.pages.len(), 2, "the duplicate page is collapsed");
        assert_eq!(d.pages[0].handle, "p1");
        assert_eq!(d.pages[1].handle, "p2");
        assert!(d.pages[0].headings.iter().any(|h| h == "Install"));
        assert!(d.pages[1].headings.iter().any(|h| h == "Usage"));
        assert!(!d.pages[0].card.contains(common));
        assert!(!d.pages[1].card.contains(common));
        assert!(d.boilerplate_removed > 0);
        assert!(d.digest_chars < d.read_chars);
        assert!(d.text.contains("crawl get p1"));
    }

    #[test]
    fn focus_line_names_topics_and_never_divides_by_zero() {
        let d = Digest {
            host: "ex.com".into(),
            seed: "https://ex.com/".into(),
            job: Some("j1".into()),
            pages: Vec::new(),
            read_chars: 0,
            digest_chars: 0,
            boilerplate_removed: 0,
            collapsed: 0,
            focus: Some("channels select shared state".into()),
            sections_covered: 1,
            sections_total: 3,
            families_covered: 1,
            families_total: 5,
            planned: 3,
            inventory_urls: 18,
            reserves: 2,
            url_weight: 0.6,
            focus_matches: 3,
            focus_precision: 1.0,
            topics_answered: 3,
            topics_total: 4,
            topics_unanswered: vec!["quantum".into()],
            stop: "covered".into(),
            wall: None,
            text: String::new(),
        };
        let text = render(&d);
        assert!(text.contains("3/4 topics answered"), "{text}");
        assert!(text.contains("quantum — no inventory page carries the term"));
        assert!(
            !text.contains("/0 pages"),
            "a zero denominator is a display bug, got {text}"
        );
        assert!(text.contains("3 of 18 inventory pages planned"));
    }
}
