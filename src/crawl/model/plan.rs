//! The planner: which pages buy the most evidence for a page budget.
//!
//! Pure: the map hands over the site's families and sections; the planner
//! returns ordered rows with a reason each, a reserve list, coverage and a
//! cost estimate. No network, no state, no page fetch. The executor (M3)
//! spends the plan; `replace_failed` is the contract for a page that dies.

use super::infer::{Family, InvUrl, Section, SiteKind};
use crate::crawl::score::{self, FocusIdf};
use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::time::Instant;
use url::Url;

/// A page's estimated output cost before fetching. The digest grows with
/// the page, but the plan needs one number that makes 10 pages comparable.
pub const TOKENS_PER_PAGE: usize = 1200;
/// Reserve rows kept for a failed page. Enough for a run that loses a
/// quarter of its budget without a second planning pass.
const RESERVE_CAP: usize = 24;
/// Explain mode's candidate table is bounded: the agent gets the shape,
/// not 14k rows.
const TRACE_CAP: usize = 200;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanKind {
    Cover,
    Deepen,
    Reserve,
}

impl PlanKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Cover => "cover",
            Self::Deepen => "deepen",
            Self::Reserve => "reserve",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanStop {
    /// Every selectable section/family is represented before the budget
    /// ran out.
    Covered,
    /// The page budget is spent.
    Budget,
    /// The scope has no selectable family (or no focus matches and the
    /// fallback also found nothing).
    Empty,
}

impl PlanStop {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Covered => "covered",
            Self::Budget => "budget",
            Self::Empty => "empty",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlanRow {
    pub url: String,
    pub section: String,
    pub template: String,
    pub kind: PlanKind,
    /// Every row says why it is here: section cover, family cover, deepen,
    /// reserve, or replacement.
    pub reason: String,
    pub score: f64,
    pub family_size: usize,
    pub section_urls: usize,
    pub est_tokens: usize,
    #[serde(default)]
    pub freshness: Option<String>,
    pub focus: bool,
    pub request_class: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PlanCoverage {
    pub sections_total: usize,
    pub sections_covered: usize,
    pub families_total: usize,
    pub families_covered: usize,
    pub urls_total: usize,
    pub urls_covered: usize,
    /// Families in the focus pool before any fallback.
    pub focus_pool: usize,
    pub focus_matches: usize,
    pub focus_precision: f64,
    /// URL-weighted section coverage: one page in a section credits the
    /// section's whole family size. The gate's unit.
    pub url_weight: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlanTrace {
    pub family: String,
    pub section: String,
    pub template: String,
    pub size: usize,
    pub score: f64,
    pub focus: bool,
    /// `page`, `reserve`, or `budget`.
    pub status: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Plan {
    pub seed: String,
    pub host: String,
    pub site_kind: SiteKind,
    pub focus: Option<String>,
    pub budget_pages: usize,
    pub pages: Vec<PlanRow>,
    pub reserves: Vec<PlanRow>,
    pub coverage: PlanCoverage,
    pub estimated_tokens: usize,
    pub stop: PlanStop,
    /// Planning time only, not the map's inventory reads.
    pub elapsed_ms: u64,
    /// True when the focus matched nothing and the plan fell back to
    /// breadth. Focus precision is then 0, stated rather than hidden.
    pub focus_fallback: bool,
    #[serde(default)]
    pub trace: Vec<PlanTrace>,
    #[serde(default)]
    pub trace_truncated: bool,
    #[serde(default)]
    section_urls: BTreeMap<String, usize>,
    #[serde(default)]
    sections_total: usize,
    #[serde(default)]
    families_total: usize,
    #[serde(default)]
    urls_total: usize,
}

pub struct PlanRequest<'a> {
    pub seed: &'a str,
    pub host: &'a str,
    pub site_kind: SiteKind,
    pub focus: Option<&'a str>,
    pub max_pages: usize,
    pub families: &'a [Family],
    pub sections: &'a [Section],
    /// The full in-scope inventory. Focus plans need every URL, not the
    /// capped map view; a family's three samples are a fallback.
    pub urls: &'a [InvUrl],
    /// Include the bounded candidate table (the CLI `explain` verb).
    pub explain: bool,
}

/// One family's representative URL and score. Owned because the plan
/// sorts and reuses it after the input borrow ends.
struct Candidate {
    family: usize,
    section: usize,
    url: String,
    samples: Vec<String>,
    focus: bool,
    score: f64,
}

/// Build a plan. Deterministic for the same model and options.
pub fn build(req: PlanRequest<'_>) -> Plan {
    let started = Instant::now();
    let budget = req.max_pages.clamp(1, 200);
    let families: Vec<Family> = req.families.to_vec();
    let sections: Vec<Section> = req.sections.to_vec();
    let section_index: HashMap<String, usize> = sections
        .iter()
        .enumerate()
        .map(|(i, s)| (s.path.clone(), i))
        .collect();
    let focus = req.focus.map(str::trim).filter(|f| !f.is_empty());
    let idf = focus.map(|_q| {
        FocusIdf::from_paths(
            families
                .iter()
                .map(|f| format!("/{}/{}", f.section, f.template)),
        )
    });

    // Every focus URL, grouped by its most-specific section. The full
    // inventory is used, not the capped map view: a 10-page plan should
    // not inherit a ranking cut made for a 120-URL preview.
    let focus_by_section: HashMap<String, Vec<String>> = if let Some(q) = focus {
        let section_paths: HashSet<&str> = sections.iter().map(|s| s.path.as_str()).collect();
        let max_depth = sections
            .iter()
            .map(|s| s.path.split('/').count())
            .max()
            .unwrap_or(0);
        let mut by_section: HashMap<String, Vec<String>> = HashMap::new();
        for entry in req.urls {
            let url = &entry.url;
            if !url_matches(url, q) {
                continue;
            }
            if let Some(section) = section_for_url(url, &section_paths, max_depth) {
                by_section.entry(section).or_default().push(url.clone());
            }
        }
        by_section
    } else {
        HashMap::new()
    };

    let mut focus_pool = 0usize;
    let mut candidates = candidate_pool(
        &families,
        &sections,
        &section_index,
        &focus_by_section,
        focus,
        idf.as_ref(),
        req.site_kind,
        focus.is_some(),
        &mut focus_pool,
    );
    let mut focus_fallback = false;
    if focus.is_some() && candidates.is_empty() {
        focus_fallback = true;
        focus_pool = 0;
        candidates = candidate_pool(
            &families,
            &sections,
            &section_index,
            &focus_by_section,
            None,
            None,
            req.site_kind,
            false,
            &mut focus_pool,
        );
    }

    // Site-kind ordering of sections: focus first when set, then the
    // kind's own breadth order (large products for docs/catalogs, newest
    // archive for blogs).
    let mut section_best: HashMap<usize, f64> = HashMap::new();
    let mut by_section: HashMap<usize, Vec<usize>> = HashMap::new();
    let mut order: Vec<usize> = (0..candidates.len()).collect();
    order.sort_by(|&a, &b| {
        candidates[b]
            .score
            .partial_cmp(&candidates[a].score)
            .unwrap_or(Ordering::Equal)
            .then_with(|| {
                sections[candidates[b].section]
                    .urls
                    .cmp(&sections[candidates[a].section].urls)
            })
            .then_with(|| candidates[a].url.cmp(&candidates[b].url))
    });
    for &ci in &order {
        let si = candidates[ci].section;
        section_best
            .entry(si)
            .and_modify(|s| *s = s.max(candidates[ci].score))
            .or_insert(candidates[ci].score);
        by_section.entry(si).or_default().push(ci);
    }
    let mut section_order: Vec<usize> = by_section.keys().copied().collect();
    section_order.sort_by(|&a, &b| {
        let focus_cmp = if focus.is_some() {
            section_best
                .get(&b)
                .copied()
                .unwrap_or(0.0)
                .partial_cmp(&section_best.get(&a).copied().unwrap_or(0.0))
                .unwrap_or(Ordering::Equal)
        } else {
            Ordering::Equal
        };
        focus_cmp.then_with(|| section_cmp(req.site_kind, &sections[a], &sections[b]))
    });

    let mut pages: Vec<PlanRow> = Vec::with_capacity(budget);
    let mut reserves: Vec<PlanRow> = Vec::new();
    let mut used_urls: HashSet<String> = HashSet::new();
    let mut used_families: HashSet<usize> = HashSet::new();
    let mut selected: Vec<usize> = Vec::new();

    // 1. Cover: one page per active section first. This is the coverage
    // unit; a second page in the same section is depth, not breadth.
    for si in section_order {
        if pages.len() >= budget {
            break;
        }
        let Some(list) = by_section.get(&si) else {
            continue;
        };
        if let Some(&ci) = list.iter().find(|&&ci| {
            !used_families.contains(&candidates[ci].family)
                && !used_urls.contains(&candidates[ci].url)
        }) {
            push_page(
                &mut pages,
                &mut used_urls,
                &mut used_families,
                &mut selected,
                ci,
                &candidates,
                &families,
                &sections,
                PlanKind::Cover,
                format!("cover section {}", sections[si].path),
                candidates[ci].focus,
            );
        }
    }

    // 2. Family cover: every remaining family gets one page before any
    // family gets a second one.
    for &ci in &order {
        if pages.len() >= budget {
            break;
        }
        if used_families.contains(&candidates[ci].family) || used_urls.contains(&candidates[ci].url)
        {
            continue;
        }
        push_page(
            &mut pages,
            &mut used_urls,
            &mut used_families,
            &mut selected,
            ci,
            &candidates,
            &families,
            &sections,
            PlanKind::Cover,
            format!(
                "cover family {}/{}",
                families[candidates[ci].family].section, families[candidates[ci].family].template
            ),
            candidates[ci].focus,
        );
    }

    // 3. Deepen: only after every selectable family has a page. Extra
    // samples from already-covered families; focus samples stay focus.
    let deepen_order = selected.clone();
    for ci in deepen_order {
        if pages.len() >= budget {
            break;
        }
        let c = &candidates[ci];
        for sample in &c.samples {
            if pages.len() >= budget {
                break;
            }
            if sample == &c.url || used_urls.contains(sample) {
                continue;
            }
            if let Some(q) = focus
                && !url_matches(sample, q)
            {
                continue;
            }
            let family = &families[c.family];
            let row = make_row(
                c,
                family,
                &sections[c.section],
                sample.clone(),
                PlanKind::Deepen,
                format!(
                    "deepen {}/{} (page {})",
                    family.section,
                    family.template,
                    pages.len() + 1
                ),
                focus.is_some(),
            );
            used_urls.insert(row.url.clone());
            pages.push(row);
        }
    }

    // 4. Reserves: the next family reps and unused samples, in score
    // order. A failed page is replaced from here without a second map.
    for &ci in &order {
        if reserves.len() >= RESERVE_CAP {
            break;
        }
        let c = &candidates[ci];
        let family = &families[c.family];
        if !used_urls.contains(&c.url) {
            let row = make_row(
                c,
                family,
                &sections[c.section],
                c.url.clone(),
                PlanKind::Reserve,
                format!("reserve: {}/{}", family.section, family.template),
                c.focus,
            );
            used_urls.insert(row.url.clone());
            reserves.push(row);
        }
        for sample in &c.samples {
            if reserves.len() >= RESERVE_CAP {
                break;
            }
            if sample == &c.url || used_urls.contains(sample) {
                continue;
            }
            if let Some(q) = focus
                && !url_matches(sample, q)
            {
                continue;
            }
            let row = make_row(
                c,
                family,
                &sections[c.section],
                sample.clone(),
                PlanKind::Reserve,
                format!("reserve: deepen {}/{}", family.section, family.template),
                focus.is_some(),
            );
            used_urls.insert(row.url.clone());
            reserves.push(row);
        }
    }

    let section_urls: BTreeMap<String, usize> =
        sections.iter().map(|s| (s.path.clone(), s.urls)).collect();
    let urls_total: usize = sections.iter().map(|s| s.urls).sum();
    let sections_total = sections.len();
    let families_total = families.len();
    let coverage = coverage_of(
        &pages,
        &section_urls,
        sections_total,
        families_total,
        urls_total,
        focus_pool,
    );

    let stop = if pages.len() >= budget {
        PlanStop::Budget
    } else if pages.is_empty() {
        PlanStop::Empty
    } else {
        PlanStop::Covered
    };

    let mut trace = Vec::new();
    let mut trace_truncated = false;
    if req.explain {
        for &ci in &order {
            if trace.len() >= TRACE_CAP {
                trace_truncated = true;
                break;
            }
            let c = &candidates[ci];
            let family = &families[c.family];
            let status = if used_families.contains(&c.family) {
                "page"
            } else if reserves.iter().any(|r| r.url == c.url) {
                "reserve"
            } else if pages.len() >= budget {
                "budget"
            } else {
                "candidate"
            };
            trace.push(PlanTrace {
                family: format!("{}/{}", family.section, family.template),
                section: family.section.clone(),
                template: family.template.clone(),
                size: family.size,
                score: round(c.score),
                focus: c.focus,
                status: status.to_string(),
            });
        }
    }

    Plan {
        seed: req.seed.to_string(),
        host: req.host.to_string(),
        site_kind: req.site_kind,
        focus: focus.map(str::to_string),
        budget_pages: budget,
        pages,
        reserves,
        coverage,
        estimated_tokens: 0, // recomputed below from the final page list
        stop,
        elapsed_ms: 0,
        focus_fallback,
        trace,
        trace_truncated,
        section_urls,
        sections_total,
        families_total,
        urls_total,
    }
    .finish(started)
}

impl Plan {
    fn finish(mut self, started: Instant) -> Self {
        self.recompute();
        self.elapsed_ms = started.elapsed().as_millis() as u64;
        self
    }

    /// Replace a failed planned URL from the reserve list. Prefers a
    /// reserve in the same section, then the same family, then the best
    /// remaining reserve. Returns the row now planned, or None when the
    /// plan has no reserve left.
    pub fn replace_failed(&mut self, failed_url: &str) -> Option<PlanRow> {
        let index = self.pages.iter().position(|p| p.url == failed_url)?;
        let failed = self.pages.remove(index);
        let pos = self
            .reserves
            .iter()
            .position(|r| r.section == failed.section)
            .or_else(|| {
                self.reserves
                    .iter()
                    .position(|r| r.template == failed.template)
            })
            .or_else(|| (!self.reserves.is_empty()).then_some(0))?;
        let mut replacement = self.reserves.remove(pos);
        replacement.kind = failed.kind;
        replacement.reason = format!("replacement for {} ({})", failed.section, failed.url);
        self.pages.insert(index, replacement.clone());
        self.stop = if self.pages.len() >= self.budget_pages {
            PlanStop::Budget
        } else if self.pages.is_empty() {
            PlanStop::Empty
        } else {
            PlanStop::Covered
        };
        self.recompute();
        Some(replacement)
    }

    fn recompute(&mut self) {
        self.coverage = coverage_of(
            &self.pages,
            &self.section_urls,
            self.sections_total,
            self.families_total,
            self.urls_total,
            self.coverage.focus_pool,
        );
        self.estimated_tokens = self.pages.len() * TOKENS_PER_PAGE;
    }
}

#[allow(clippy::too_many_arguments)]
fn candidate_pool(
    families: &[Family],
    sections: &[Section],
    section_index: &HashMap<String, usize>,
    focus_by_section: &HashMap<String, Vec<String>>,
    focus: Option<&str>,
    idf: Option<&FocusIdf>,
    kind: SiteKind,
    filter_focus: bool,
    focus_pool: &mut usize,
) -> Vec<Candidate> {
    let mut out = Vec::new();
    let mut focus_cursor: HashMap<String, usize> = HashMap::new();
    let mut used_focus_urls: HashSet<String> = HashSet::new();
    for (fi, family) in families.iter().enumerate() {
        let Some(&si) = section_index.get(&family.section) else {
            continue;
        };
        // A focused pool only scores sections that have a matching URL;
        // on a 14k-section site this is the difference between 14k sample
        // clones and the handful that can be planned.
        if filter_focus
            && !focus_by_section.is_empty()
            && !focus_by_section.contains_key(&family.section)
        {
            continue;
        }
        let samples: Vec<String> = family
            .sample
            .iter()
            .filter(|u| Url::parse(u).is_ok())
            .cloned()
            .collect();
        if samples.is_empty() {
            continue;
        }
        let matching = focus.and_then(|q| {
            if let Some(url) = samples.iter().find(|u| url_matches(u, q)).cloned()
                && used_focus_urls.insert(url.clone())
            {
                return Some(url);
            }
            let list = focus_by_section.get(&family.section)?;
            let cursor = focus_cursor.entry(family.section.clone()).or_insert(0);
            while let Some(url) = list.get(*cursor) {
                *cursor += 1;
                if used_focus_urls.insert(url.clone()) {
                    return Some(url.clone());
                }
            }
            None
        });
        let (url, is_focus) = if filter_focus {
            let Some(url) = matching else {
                continue;
            };
            *focus_pool += 1;
            (url, true)
        } else {
            (samples[0].clone(), false)
        };
        let score = family_score(family, &sections[si], focus, idf, kind);
        out.push(Candidate {
            family: fi,
            section: si,
            url,
            samples,
            focus: is_focus,
            score,
        });
    }
    out
}

#[allow(clippy::too_many_arguments)]
fn push_page(
    pages: &mut Vec<PlanRow>,
    used_urls: &mut HashSet<String>,
    used_families: &mut HashSet<usize>,
    selected: &mut Vec<usize>,
    ci: usize,
    candidates: &[Candidate],
    families: &[Family],
    sections: &[Section],
    kind: PlanKind,
    reason: String,
    focus: bool,
) {
    let c = &candidates[ci];
    let row = make_row(
        c,
        &families[c.family],
        &sections[c.section],
        c.url.clone(),
        kind,
        reason,
        focus,
    );
    used_urls.insert(row.url.clone());
    used_families.insert(c.family);
    pages.push(row);
    selected.push(ci);
}

#[allow(clippy::too_many_arguments)]
fn make_row(
    c: &Candidate,
    family: &Family,
    section: &Section,
    url: String,
    kind: PlanKind,
    reason: String,
    focus: bool,
) -> PlanRow {
    PlanRow {
        url,
        section: family.section.clone(),
        template: family.template.clone(),
        kind,
        reason,
        score: round(c.score),
        family_size: family.size,
        section_urls: section.urls,
        est_tokens: TOKENS_PER_PAGE,
        freshness: family.lastmod.clone().or_else(|| section.lastmod.clone()),
        focus,
        request_class: "content".to_string(),
    }
}

fn family_score(
    family: &Family,
    section: &Section,
    focus: Option<&str>,
    idf: Option<&FocusIdf>,
    kind: SiteKind,
) -> f64 {
    let mut score = 1.0;
    if let Some(q) = focus {
        let best = score::score_candidate_with_idf(
            "",
            &format!("/{}/{}", family.section, family.template),
            Some(q),
            idf,
        );
        score += best * 2.0;
    }
    // A bigger section is worth more coverage; a leaf template is a
    // better representative than a listing.
    score += (section.urls.max(1) as f64).ln() * 0.15;
    if family.template.ends_with(":v") {
        score += 0.3;
    } else {
        score -= 0.1;
    }
    if listing_template(&family.template) {
        score -= 0.5;
    }
    if kind == SiteKind::Blog {
        score += date_rank(section) * 0.5;
    }
    score
}

fn listing_template(template: &str) -> bool {
    template.split('/').any(|s| {
        matches!(
            s.to_ascii_lowercase().as_str(),
            "tag"
                | "tags"
                | "category"
                | "categories"
                | "archive"
                | "archives"
                | "page"
                | "index"
                | "search"
                | "author"
                | "authors"
        )
    })
}

fn section_cmp(kind: SiteKind, a: &Section, b: &Section) -> Ordering {
    let by_kind = match kind {
        SiteKind::Blog => date_key(b)
            .cmp(&date_key(a))
            .then_with(|| b.urls.cmp(&a.urls)),
        _ => b.urls.cmp(&a.urls),
    };
    by_kind.then_with(|| a.path.cmp(&b.path))
}

fn date_key(section: &Section) -> String {
    if section.date
        && let Some(first) = section.path.split('/').next()
    {
        let digits: String = first.chars().filter(char::is_ascii_digit).collect();
        if digits.len() >= 4 {
            return digits;
        }
    }
    section.lastmod.clone().unwrap_or_default()
}

fn date_rank(section: &Section) -> f64 {
    let key = date_key(section);
    let year = key.get(0..4).and_then(|y| y.parse::<u32>().ok());
    year.map(|y| ((y.saturating_sub(2000)) as f64 / 50.0).clamp(0.0, 1.0))
        .unwrap_or(0.0)
}

fn url_matches(url: &str, focus: &str) -> bool {
    Url::parse(url)
        .map(|u| score::focus_match("", u.path(), focus))
        .unwrap_or(false)
}

fn section_for_url(url: &str, sections: &HashSet<&str>, max_depth: usize) -> Option<String> {
    let u = Url::parse(url).ok()?;
    let segments: Vec<&str> = u
        .path()
        .trim_start_matches('/')
        .split('/')
        .filter(|s| !s.is_empty())
        .collect();
    if segments.is_empty() {
        return sections.contains("root").then(|| "root".to_string());
    }
    for depth in (1..=segments.len().min(max_depth)).rev() {
        let candidate = segments[..depth].join("/");
        if sections.contains(candidate.as_str()) {
            return Some(candidate);
        }
    }
    None
}

fn coverage_of(
    pages: &[PlanRow],
    section_urls: &BTreeMap<String, usize>,
    sections_total: usize,
    families_total: usize,
    urls_total: usize,
    focus_pool: usize,
) -> PlanCoverage {
    let mut sections: HashSet<&str> = HashSet::new();
    let mut families: HashSet<(&str, &str)> = HashSet::new();
    let mut focus_matches = 0usize;
    for row in pages {
        sections.insert(row.section.as_str());
        families.insert((row.section.as_str(), row.template.as_str()));
        if row.focus {
            focus_matches += 1;
        }
    }
    let urls_covered: usize = sections
        .iter()
        .filter_map(|s| section_urls.get(*s).copied())
        .sum();
    PlanCoverage {
        sections_total,
        sections_covered: sections.len(),
        families_total,
        families_covered: families.len(),
        urls_total,
        urls_covered,
        focus_pool,
        focus_matches,
        focus_precision: if pages.is_empty() {
            0.0
        } else {
            focus_matches as f64 / pages.len() as f64
        },
        url_weight: if urls_total == 0 {
            0.0
        } else {
            urls_covered as f64 / urls_total as f64
        },
    }
}

fn round(n: f64) -> f64 {
    (n * 1000.0).round() / 1000.0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crawl::model::infer::sections_from_families;

    fn fam(section: &str, template: &str, size: usize, samples: &[&str]) -> Family {
        Family {
            section: section.to_string(),
            template: template.to_string(),
            size,
            lastmod: None,
            sample: samples.iter().map(|s| (*s).to_string()).collect(),
        }
    }

    fn build_plan(
        families: &[Family],
        kind: SiteKind,
        focus: Option<&str>,
        max_pages: usize,
        explain: bool,
    ) -> Plan {
        let sections = sections_from_families(families);
        build(PlanRequest {
            seed: "https://ex.com/docs/",
            host: "ex.com",
            site_kind: kind,
            focus,
            max_pages,
            families,
            sections: &sections,
            urls: &[],
            explain,
        })
    }

    #[test]
    fn docs_cover_distinct_sections_before_depth() {
        let families = vec![
            fam(
                "web/api",
                ":v",
                10,
                &["https://ex.com/web/api/a", "https://ex.com/web/api/b"],
            ),
            fam(
                "web/css",
                ":v",
                8,
                &["https://ex.com/web/css/a", "https://ex.com/web/css/b"],
            ),
            fam(
                "web/html",
                ":v",
                6,
                &["https://ex.com/web/html/a", "https://ex.com/web/html/b"],
            ),
        ];
        let plan = build_plan(&families, SiteKind::Docs, None, 3, false);
        assert_eq!(plan.pages.len(), 3);
        assert_eq!(plan.coverage.sections_covered, 3);
        assert_eq!(plan.coverage.url_weight, 1.0);
        assert!(plan.pages.iter().all(|p| p.reason.starts_with("cover")));
        assert_eq!(plan.stop, PlanStop::Budget);
    }

    #[test]
    fn a_budget_plan_has_reserves_and_every_row_has_a_reason() {
        let families: Vec<Family> = (0..8)
            .map(|i| {
                fam(
                    &format!("s{i}"),
                    ":v",
                    2,
                    &[
                        &format!("https://ex.com/s{i}/a"),
                        &format!("https://ex.com/s{i}/b"),
                    ],
                )
            })
            .collect();
        let plan = build_plan(&families, SiteKind::Docs, None, 3, false);
        assert_eq!(plan.pages.len(), 3);
        assert!(!plan.reserves.is_empty());
        assert!(plan.pages.iter().all(|p| !p.reason.is_empty()));
        assert!(plan.reserves.iter().all(|p| !p.reason.is_empty()));
        assert_eq!(plan.stop, PlanStop::Budget);
    }

    #[test]
    fn focus_keeps_only_matching_urls() {
        let families = vec![
            fam(
                "webhooks",
                ":v",
                2,
                &["https://ex.com/webhooks/a", "https://ex.com/webhooks/b"],
            ),
            fam(
                "blog",
                ":v",
                2,
                &["https://ex.com/blog/x", "https://ex.com/blog/y"],
            ),
        ];
        let plan = build_plan(&families, SiteKind::Docs, Some("webhooks"), 10, false);
        assert_eq!(plan.pages.len(), 2);
        assert_eq!(plan.coverage.focus_pool, 1);
        assert_eq!(plan.coverage.focus_matches, 2);
        assert_eq!(plan.coverage.focus_precision, 1.0);
        assert!(plan.pages.iter().all(|p| p.focus));
    }

    #[test]
    fn a_blog_starts_with_the_newest_year() {
        let families = vec![
            fam(
                "2025",
                ":v",
                2,
                &["https://ex.com/2025/a", "https://ex.com/2025/b"],
            ),
            fam(
                "2026",
                ":v",
                2,
                &["https://ex.com/2026/a", "https://ex.com/2026/b"],
            ),
        ];
        let plan = build_plan(&families, SiteKind::Blog, None, 1, false);
        assert_eq!(plan.pages.len(), 1);
        assert_eq!(plan.pages[0].section, "2026");
    }

    #[test]
    fn a_failed_page_is_replaced_from_the_reserves() {
        let families: Vec<Family> = (0..8)
            .map(|i| {
                fam(
                    &format!("s{i}"),
                    ":v",
                    2,
                    &[
                        &format!("https://ex.com/s{i}/a"),
                        &format!("https://ex.com/s{i}/b"),
                    ],
                )
            })
            .collect();
        let mut plan = build_plan(&families, SiteKind::Docs, None, 3, false);
        let failed = plan.pages[0].url.clone();
        let failed_section = plan.pages[0].section.clone();
        let before = plan
            .pages
            .iter()
            .map(|p| p.url.clone())
            .collect::<HashSet<_>>();
        let replacement = plan.replace_failed(&failed).expect("reserve exists");
        assert_eq!(plan.pages.len(), 3);
        assert!(!before.contains(&replacement.url));
        assert!(plan.pages.iter().any(|p| p.url == replacement.url));
        assert!(replacement.reason.contains("replacement"));
        assert_eq!(plan.coverage.sections_covered, 3);
        assert_eq!(replacement.section, failed_section);
    }

    #[test]
    fn explain_includes_a_bounded_candidate_table() {
        let families: Vec<Family> = (0..5)
            .map(|i| {
                fam(
                    &format!("s{i}"),
                    ":v",
                    1,
                    &[&format!("https://ex.com/s{i}/a")],
                )
            })
            .collect();
        let plan = build_plan(&families, SiteKind::Docs, None, 3, true);
        assert!(!plan.trace.is_empty());
        assert!(plan.trace.iter().any(|t| t.status == "page"));
    }

    #[test]
    fn an_empty_model_is_empty() {
        let plan = build_plan(&[], SiteKind::Docs, None, 10, false);
        assert_eq!(plan.stop, PlanStop::Empty);
        assert!(plan.pages.is_empty());
        assert!(plan.reserves.is_empty());
    }

    #[test]
    fn a_focus_with_no_match_falls_back_and_says_so() {
        let families = vec![fam("docs", ":v", 1, &["https://ex.com/docs/a"])];
        let plan = build_plan(
            &families,
            SiteKind::Docs,
            Some("nonexistent-topic"),
            3,
            false,
        );
        assert!(plan.focus_fallback);
        assert_eq!(plan.coverage.focus_precision, 0.0);
        assert!(!plan.pages.is_empty());
    }
}
