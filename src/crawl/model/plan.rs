//! The planner: which pages buy the most evidence for a page budget.
//!
//! Pure: the map hands over the site's families and sections; the planner
//! returns ordered rows with a reason each, a reserve list, coverage and a
//! cost estimate. No network, no state, no page fetch. The executor (M3)
//! spends the plan; `replace_failed` is the contract for a page that dies.

use super::infer::{Family, InvUrl, Section, SiteKind};
use crate::crawl::score::{self, FocusIdf};
use crate::extract::{focus as lexicon, language};
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
/// Members kept per family. A family is a set of pages; this bound is
/// memory, not policy.
const MEMBER_CAP: usize = 64;
/// Focus topics a plan answers. The caller's own words, bounded.
const TOPIC_CAP: usize = 8;
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
    /// The caller's focus topics this page answers, by label. A multi-topic
    /// focus is planned per topic, so a row names what it buys.
    #[serde(default)]
    pub topics: Vec<String>,
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
    /// Pages this plan actually returns. `urls_covered` is section-credited
    /// (one page speaks for its whole section); this is the honest count.
    #[serde(default)]
    pub urls_planned: usize,
    /// Families in the focus pool before any fallback.
    pub focus_pool: usize,
    /// URLs in the focus pool: every inventory page that answers the focus.
    #[serde(default)]
    pub focus_urls: usize,
    pub focus_matches: usize,
    pub focus_precision: f64,
    /// The caller's focus topics: terms the plan owes an answer, not words
    /// scored against a family representative.
    #[serde(default)]
    pub focus_topics: usize,
    #[serde(default)]
    pub focus_topics_answered: usize,
    /// Topics no planned page answers. An empty list is the recall claim;
    /// a non-empty one names the gap instead of hiding it behind `covered`.
    #[serde(default)]
    pub focus_unanswered: Vec<String>,
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
    /// Focus URLs in the pool and the labels it owes an answer for, kept
    /// so `recompute` re-derives coverage after a failed-page replacement.
    #[serde(default)]
    focus_urls: usize,
    #[serde(default)]
    focus_labels: Vec<String>,
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
///
/// A family is a **set**, not a page: `members` carries the rest of the
/// inventory URLs this family covers, best first, so depth can spend the
/// remaining budget on them instead of stopping at a three-URL sample.
struct Candidate {
    family: usize,
    section: usize,
    url: String,
    /// Focus topics the representative answers.
    topics: u32,
    members: Vec<Member>,
    focus: bool,
    score: f64,
    /// The representative's own focus score, without the family prior.
    best_score: f64,
}

/// One inventory page inside a family: what it scores for the focus and
/// which of the caller's topics it answers.
struct Member {
    url: String,
    score: f64,
    topics: u32,
    lastmod: Option<String>,
}

/// One term of the caller's focus string. The label is the caller's own
/// word; the tokens are its stemmed forms, matched against a URL path.
#[derive(Debug, Clone)]
struct Topic {
    label: String,
    tokens: Vec<String>,
}

/// `best_topic_member` uses this to name a candidate's own URL.
const OWN: usize = usize::MAX;

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

    // The family is a set, not a page. Every in-scope URL is assigned to
    // the family its shape belongs to, so a family's members are all of its
    // pages rather than a three-URL sample. A focus plan scores those
    // members and answers each named topic before it spends a page on
    // breadth; whatever the budget leaves is spent on members, not dropped.
    let topics = focus.map(topics_of).unwrap_or_default();
    let mut by_section_path: HashMap<&str, Vec<usize>> = HashMap::new();
    for (fi, f) in families.iter().enumerate() {
        by_section_path
            .entry(f.section.as_str())
            .or_default()
            .push(fi);
    }
    let (mut candidates, mut focus_pool, focus_urls) = build_candidates(
        &families,
        &sections,
        &section_index,
        &by_section_path,
        req.urls,
        focus,
        idf.as_ref(),
        &topics,
        req.site_kind,
    );
    let mut focus_fallback = false;
    if focus.is_some() && candidates.is_empty() {
        focus_fallback = true;
        focus_pool = 0;
        let (fallback, _, _) = build_candidates(
            &families,
            &sections,
            &section_index,
            &by_section_path,
            req.urls,
            None,
            None,
            &[],
            req.site_kind,
        );
        candidates = fallback;
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
    let mut covered_sections: HashSet<usize> = HashSet::new();

    // 0. The caller's topics first. Every named term the inventory can
    // answer gets a page before breadth spends one: the recall unit is the
    // topic, never a family representative that happens to carry a word.
    for ti in 0..topics.len() {
        if pages.len() >= budget {
            break;
        }
        let label = topics[ti].label.clone();
        if pages.iter().any(|r| r.topics.contains(&label)) {
            continue;
        }
        let Some((ci, mi)) = best_topic_member(&candidates, ti, &used_urls) else {
            continue;
        };
        let (url, mask, score) = member_at(&candidates[ci], mi);
        push_row(
            &mut pages,
            &mut used_urls,
            &mut used_families,
            &mut covered_sections,
            &candidates,
            ci,
            url,
            mask,
            score,
            &families,
            &sections,
            &topics,
            PlanKind::Cover,
            format!("focus topic \"{label}\""),
            true,
        );
    }

    // 1. Cover: one page per section the plan has not represented yet.
    for &si in &section_order {
        if pages.len() >= budget {
            break;
        }
        if covered_sections.contains(&si) {
            continue;
        }
        let Some(list) = by_section.get(&si) else {
            continue;
        };
        if let Some(&ci) = list
            .iter()
            .find(|&&ci| !used_urls.contains(&candidates[ci].url))
        {
            push_page(
                &mut pages,
                &mut used_urls,
                &mut used_families,
                &mut covered_sections,
                &candidates,
                ci,
                &families,
                &sections,
                &topics,
                PlanKind::Cover,
                format!("cover section {}", sections[si].path),
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
            &mut covered_sections,
            &candidates,
            ci,
            &families,
            &sections,
            &topics,
            PlanKind::Cover,
            format!(
                "cover family {}/{}",
                families[candidates[ci].family].section, families[candidates[ci].family].template
            ),
        );
    }

    // 3. Deepen: after breadth the remaining budget goes to the members of
    // the families already represented, best first. `tokio/tutorial/:v` is
    // ten pages, not a representative plus two samples.
    for &ci in &order {
        if pages.len() >= budget {
            break;
        }
        let mut mi = 0;
        while mi < candidates[ci].members.len() && pages.len() < budget {
            let (url, mask, score) = member_at(&candidates[ci], mi);
            mi += 1;
            if used_urls.contains(&url) {
                continue;
            }
            let family = &families[candidates[ci].family];
            let page_no = pages.len() + 1;
            push_row(
                &mut pages,
                &mut used_urls,
                &mut used_families,
                &mut covered_sections,
                &candidates,
                ci,
                url,
                mask,
                score,
                &families,
                &sections,
                &topics,
                PlanKind::Deepen,
                format!(
                    "deepen {}/{} (page {})",
                    family.section, family.template, page_no
                ),
                candidates[ci].focus,
            );
        }
    }

    // 4. Reserves: family reps and members the budget did not reach, in
    // score order. A failed page is replaced from here without a second map.
    for &ci in &order {
        if reserves.len() >= RESERVE_CAP {
            break;
        }
        let c = &candidates[ci];
        let family = &families[c.family];
        if !used_urls.contains(&c.url) {
            let row = make_row(
                family,
                &sections[c.section],
                c.url.clone(),
                c.topics,
                c.best_score,
                PlanKind::Reserve,
                format!("reserve: {}/{}", family.section, family.template),
                c.focus,
                &topics,
            );
            used_urls.insert(row.url.clone());
            reserves.push(row);
        }
        for member in &c.members {
            if reserves.len() >= RESERVE_CAP {
                break;
            }
            if used_urls.contains(&member.url) {
                continue;
            }
            let row = make_row(
                family,
                &sections[c.section],
                member.url.clone(),
                member.topics,
                member.score,
                PlanKind::Reserve,
                format!("reserve: deepen {}/{}", family.section, family.template),
                c.focus,
                &topics,
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
        focus_urls,
        &topics,
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
        focus_urls,
        focus_labels: topics.iter().map(|t| t.label.clone()).collect(),
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
        let topics: Vec<Topic> = self
            .focus_labels
            .iter()
            .map(|label| Topic {
                label: label.clone(),
                tokens: Vec::new(),
            })
            .collect();
        self.coverage = coverage_of(
            &self.pages,
            &self.section_urls,
            self.sections_total,
            self.families_total,
            self.urls_total,
            self.coverage.focus_pool,
            self.focus_urls,
            &topics,
        );
        self.estimated_tokens = self.pages.len() * TOKENS_PER_PAGE;
    }
}

/// Build one candidate per family from the inventory. A family's members
/// are every in-scope URL whose shape belongs to it, best first; the
/// family's samples are the fallback when the plan has no inventory (a warm
/// dossier). Returns the candidates and the focus pool's size in families
/// and URLs.
#[allow(clippy::too_many_arguments)]
fn build_candidates(
    families: &[Family],
    sections: &[Section],
    section_index: &HashMap<String, usize>,
    by_section: &HashMap<&str, Vec<usize>>,
    urls: &[InvUrl],
    focus: Option<&str>,
    idf: Option<&FocusIdf>,
    topics: &[Topic],
    kind: SiteKind,
) -> (Vec<Candidate>, usize, usize) {
    let mut raw: Vec<Vec<Member>> = (0..families.len()).map(|_| Vec::new()).collect();
    for entry in urls {
        let Some(fi) = family_for(&entry.url, families, by_section) else {
            continue;
        };
        let mask = topic_mask(&entry.url, entry.title.as_deref(), topics);
        if let Some(q) = focus
            && mask == 0
            && !url_matches(&entry.url, entry.title.as_deref(), q)
        {
            continue;
        }
        let score = match focus {
            Some(q) => score::score_candidate_with_idf(
                entry.title.as_deref().unwrap_or(""),
                path_of(&entry.url),
                Some(q),
                idf,
            ),
            None => 0.0,
        };
        raw[fi].push(Member {
            url: entry.url.clone(),
            score,
            topics: mask,
            lastmod: entry.lastmod.clone(),
        });
    }

    let mut out: Vec<Candidate> = Vec::with_capacity(families.len());
    let mut focus_pool = 0usize;
    let mut focus_urls = 0usize;
    for (fi, family) in families.iter().enumerate() {
        let Some(&si) = section_index.get(&family.section) else {
            continue;
        };
        let mut list = std::mem::take(&mut raw[fi]);
        if list.is_empty() {
            list = sample_members(family, focus, idf, topics);
        }
        list.sort_by(|a, b| {
            if focus.is_some() {
                b.score
                    .partial_cmp(&a.score)
                    .unwrap_or(Ordering::Equal)
                    .then_with(|| b.lastmod.cmp(&a.lastmod))
                    .then_with(|| a.url.cmp(&b.url))
            } else {
                b.lastmod.cmp(&a.lastmod).then_with(|| a.url.cmp(&b.url))
            }
        });
        list.dedup_by(|a, b| a.url == b.url);
        if list.is_empty() {
            continue;
        }
        if focus.is_some() {
            focus_pool += 1;
            focus_urls += list.len();
        }
        list.truncate(MEMBER_CAP);
        let best = list.remove(0);
        let mut score = family_score(family, &sections[si], focus, idf, kind);
        if focus.is_some() {
            score += best.score;
        }
        out.push(Candidate {
            family: fi,
            section: si,
            url: best.url,
            topics: best.topics,
            members: list,
            focus: focus.is_some(),
            score,
            best_score: best.score,
        });
    }
    (out, focus_pool, focus_urls)
}

/// A family's own samples, for a plan with no inventory behind it.
fn sample_members(
    family: &Family,
    focus: Option<&str>,
    idf: Option<&FocusIdf>,
    topics: &[Topic],
) -> Vec<Member> {
    let mut out = Vec::new();
    for url in &family.sample {
        if Url::parse(url).is_err() {
            continue;
        }
        let mask = topic_mask(url, None, topics);
        if let Some(q) = focus
            && mask == 0
            && !url_matches(url, None, q)
        {
            continue;
        }
        let score = match focus {
            Some(q) => score::score_candidate_with_idf("", path_of(url), Some(q), idf),
            None => 0.0,
        };
        out.push(Member {
            url: url.clone(),
            score,
            topics: mask,
            lastmod: None,
        });
    }
    out
}

/// The family a URL's shape belongs to: the longest stored section that is
/// a path prefix, then the template whose segments match. Falls back to the
/// section's own family when the inventory moved after the model was built.
fn family_for(
    url: &str,
    families: &[Family],
    by_section: &HashMap<&str, Vec<usize>>,
) -> Option<usize> {
    let u = Url::parse(url).ok()?;
    let segs: Vec<&str> = u.path().split('/').filter(|s| !s.is_empty()).collect();
    if segs.is_empty() {
        return by_section
            .get("root")
            .and_then(|list| {
                list.iter()
                    .copied()
                    .find(|&fi| families[fi].template == ":v")
            })
            .or_else(|| {
                by_section
                    .get("root")
                    .and_then(|list| list.first().copied())
            });
    }
    for depth in (1..=segs.len()).rev() {
        let prefix = segs[..depth].join("/");
        let Some(list) = by_section.get(prefix.as_str()) else {
            continue;
        };
        if let Some(&fi) = list
            .iter()
            .find(|&&fi| template_matches(&families[fi].template, &segs))
        {
            return Some(fi);
        }
        return list.first().copied();
    }
    None
}

fn template_matches(template: &str, segs: &[&str]) -> bool {
    let parts: Vec<&str> = template.split('/').filter(|s| !s.is_empty()).collect();
    parts.len() == segs.len()
        && parts
            .iter()
            .zip(segs)
            .all(|(part, seg)| *part == ":v" || part.eq_ignore_ascii_case(seg))
}

/// The caller's focus as topics: one per meaningful word, so a multi-topic
/// ask is answered topic by topic instead of collapsing into one family
/// representative.
fn topics_of(focus: &str) -> Vec<Topic> {
    let lang = language::detect_from_text(focus);
    let mut out: Vec<Topic> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for raw in focus.split(|c: char| !(c.is_alphanumeric() || c == '_')) {
        if raw.chars().count() < 3 {
            continue;
        }
        let tokens = lexicon::tokenize(raw, &lang);
        if tokens.is_empty() {
            continue;
        }
        let label = raw.to_lowercase();
        if seen.insert(label.clone()) {
            out.push(Topic { label, tokens });
        }
        if out.len() == TOPIC_CAP {
            break;
        }
    }
    if out.is_empty() {
        for token in lexicon::tokenize(focus, &lang).into_iter().take(TOPIC_CAP) {
            if seen.insert(token.clone()) {
                out.push(Topic {
                    label: token.clone(),
                    tokens: vec![token],
                });
            }
        }
    }
    out
}

/// The topic bitset one URL answers: a topic is answered when any of its
/// tokens is a token of the page's own label or its path.
fn topic_mask(url: &str, title: Option<&str>, topics: &[Topic]) -> u32 {
    if topics.is_empty() {
        return 0;
    }
    let Ok(parsed) = Url::parse(url) else {
        return 0;
    };
    let lang = language::detect_from_text(parsed.path());
    let mut text = String::new();
    if let Some(title) = title {
        text.push_str(title);
        text.push(' ');
    }
    text.push_str(&parsed.path().replace(['/', '-', '_', '.'], " "));
    let tokens = lexicon::tokenize(&text, &lang);
    let mut mask = 0u32;
    for (i, topic) in topics.iter().enumerate() {
        if topic
            .tokens
            .iter()
            .any(|needle| tokens.iter().any(|token| token == needle))
        {
            mask |= 1 << i;
        }
    }
    mask
}

fn labels(mask: u32, topics: &[Topic]) -> Vec<String> {
    topics
        .iter()
        .enumerate()
        .filter(|(i, _)| mask & (1 << i) != 0)
        .map(|(_, topic)| topic.label.clone())
        .collect()
}

/// The best unselected page for one topic: a focus page that answers it,
/// which is what makes `focus "a b c"` plan all three.
fn best_topic_member(
    candidates: &[Candidate],
    topic: usize,
    used: &HashSet<String>,
) -> Option<(usize, usize)> {
    let bit = 1u32 << topic;
    let mut best: Option<(f64, usize, usize)> = None;
    for (ci, c) in candidates.iter().enumerate() {
        let mut consider = |score: f64, mi: usize| {
            let better = match best {
                None => true,
                Some((s, _, _)) => score > s,
            };
            if better {
                best = Some((score, ci, mi));
            }
        };
        if c.topics & bit != 0 && !used.contains(&c.url) {
            consider(c.best_score, OWN);
        }
        for (mi, member) in c.members.iter().enumerate() {
            if member.topics & bit != 0 && !used.contains(&member.url) {
                consider(member.score, mi);
            }
        }
    }
    best.map(|(_, ci, mi)| (ci, mi))
}

fn member_at(c: &Candidate, mi: usize) -> (String, u32, f64) {
    if mi == OWN {
        (c.url.clone(), c.topics, c.best_score)
    } else {
        let member = &c.members[mi];
        (member.url.clone(), member.topics, member.score)
    }
}

fn path_of(url: &str) -> &str {
    url.split_once("://")
        .map(|(_, rest)| rest.find('/').map_or("/", |i| &rest[i..]))
        .unwrap_or(url)
}

#[allow(clippy::too_many_arguments)]
fn push_page(
    pages: &mut Vec<PlanRow>,
    used_urls: &mut HashSet<String>,
    used_families: &mut HashSet<usize>,
    covered_sections: &mut HashSet<usize>,
    candidates: &[Candidate],
    ci: usize,
    families: &[Family],
    sections: &[Section],
    topics: &[Topic],
    kind: PlanKind,
    reason: String,
) {
    let c = &candidates[ci];
    push_row(
        pages,
        used_urls,
        used_families,
        covered_sections,
        candidates,
        ci,
        c.url.clone(),
        c.topics,
        c.best_score,
        families,
        sections,
        topics,
        kind,
        reason,
        c.focus,
    );
}

#[allow(clippy::too_many_arguments)]
fn push_row(
    pages: &mut Vec<PlanRow>,
    used_urls: &mut HashSet<String>,
    used_families: &mut HashSet<usize>,
    covered_sections: &mut HashSet<usize>,
    candidates: &[Candidate],
    ci: usize,
    url: String,
    mask: u32,
    score: f64,
    families: &[Family],
    sections: &[Section],
    topics: &[Topic],
    kind: PlanKind,
    reason: String,
    focus: bool,
) {
    let c = &candidates[ci];
    let row = make_row(
        &families[c.family],
        &sections[c.section],
        url,
        mask,
        score,
        kind,
        reason,
        focus,
        topics,
    );
    used_urls.insert(row.url.clone());
    used_families.insert(c.family);
    covered_sections.insert(c.section);
    pages.push(row);
}

#[allow(clippy::too_many_arguments)]
fn make_row(
    family: &Family,
    section: &Section,
    url: String,
    mask: u32,
    score: f64,
    kind: PlanKind,
    reason: String,
    focus: bool,
    topics: &[Topic],
) -> PlanRow {
    PlanRow {
        url,
        section: family.section.clone(),
        template: family.template.clone(),
        kind,
        reason,
        score: round(score),
        family_size: family.size,
        section_urls: section.urls,
        est_tokens: TOKENS_PER_PAGE,
        freshness: family.lastmod.clone().or_else(|| section.lastmod.clone()),
        focus,
        topics: labels(mask, topics),
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

fn url_matches(url: &str, title: Option<&str>, focus: &str) -> bool {
    Url::parse(url)
        .map(|u| score::focus_match(title.unwrap_or(""), u.path(), focus))
        .unwrap_or(false)
}

#[allow(clippy::too_many_arguments)]
fn coverage_of(
    pages: &[PlanRow],
    section_urls: &BTreeMap<String, usize>,
    sections_total: usize,
    families_total: usize,
    urls_total: usize,
    focus_pool: usize,
    focus_urls: usize,
    topics: &[Topic],
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
    let mut answered = 0usize;
    let mut focus_unanswered: Vec<String> = Vec::new();
    for topic in topics {
        if pages
            .iter()
            .any(|row| row.topics.iter().any(|label| label == &topic.label))
        {
            answered += 1;
        } else {
            focus_unanswered.push(topic.label.clone());
        }
    }
    PlanCoverage {
        sections_total,
        sections_covered: sections.len(),
        families_total,
        families_covered: families.len(),
        urls_total,
        urls_covered,
        urls_planned: pages.len(),
        focus_pool,
        focus_urls,
        focus_matches,
        focus_precision: if pages.is_empty() {
            0.0
        } else {
            focus_matches as f64 / pages.len() as f64
        },
        focus_topics: topics.len(),
        focus_topics_answered: answered,
        focus_unanswered,
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

    fn build_with_inventory(
        families: &[Family],
        urls: &[&str],
        focus: Option<&str>,
        max_pages: usize,
    ) -> Plan {
        let sections = sections_from_families(families);
        let inventory: Vec<InvUrl> = urls
            .iter()
            .map(|url| InvUrl {
                url: (*url).to_string(),
                lastmod: None,
                source: "test".to_string(),
                title: None,
            })
            .collect();
        build(PlanRequest {
            seed: "https://ex.com/tokio/tutorial",
            host: "ex.com",
            site_kind: SiteKind::Docs,
            focus,
            max_pages,
            families,
            sections: &sections,
            urls: &inventory,
            explain: false,
        })
    }

    fn tutorial() -> (Vec<Family>, Vec<&'static str>) {
        let families = vec![fam(
            "tokio/tutorial",
            "tokio/tutorial/:v",
            10,
            &[
                "https://ex.com/tokio/tutorial/async",
                "https://ex.com/tokio/tutorial/channels",
                "https://ex.com/tokio/tutorial/framing",
            ],
        )];
        let urls = vec![
            "https://ex.com/tokio/tutorial/async",
            "https://ex.com/tokio/tutorial/channels",
            "https://ex.com/tokio/tutorial/framing",
            "https://ex.com/tokio/tutorial/hello-tokio",
            "https://ex.com/tokio/tutorial/io",
            "https://ex.com/tokio/tutorial/select",
            "https://ex.com/tokio/tutorial/setup",
            "https://ex.com/tokio/tutorial/shared-state",
            "https://ex.com/tokio/tutorial/spawning",
            "https://ex.com/tokio/tutorial/streams",
        ];
        (families, urls)
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

    #[test]
    fn a_multi_topic_focus_plans_one_page_per_named_topic() {
        // The regression this architecture exists for: channels, select and
        // shared-state are one family (`tokio/tutorial/:v`). One page per
        // family would drop shared-state however large the budget was.
        let (families, urls) = tutorial();
        let plan = build_with_inventory(&families, &urls, Some("channels select shared state"), 10);
        let planned: HashSet<&str> = plan.pages.iter().map(|p| p.url.as_str()).collect();
        for expected in ["channels", "select", "shared-state"] {
            assert!(
                planned.iter().any(|u| u.ends_with(expected)),
                "{expected} must be planned, got {planned:?}"
            );
        }
        assert_eq!(plan.coverage.focus_topics, 4);
        assert_eq!(plan.coverage.focus_topics_answered, 4);
        assert!(plan.coverage.focus_unanswered.is_empty());
        assert_eq!(
            plan.coverage.focus_urls, 3,
            "channels, select and shared-state are the focus pool"
        );
        assert_eq!(plan.pages.len(), 3, "three matching pages, nothing padded");
        assert_eq!(plan.stop, PlanStop::Covered);
    }

    #[test]
    fn family_members_fill_the_budget_instead_of_stopping_at_samples() {
        let (families, urls) = tutorial();
        let plan = build_with_inventory(&families, &urls, None, 8);
        assert_eq!(
            plan.pages.len(),
            8,
            "a 10-page family can spend an 8-page budget"
        );
        let unique: HashSet<&str> = plan.pages.iter().map(|p| p.url.as_str()).collect();
        assert_eq!(unique.len(), 8, "no page selected twice");
        assert_eq!(plan.stop, PlanStop::Budget);
    }

    #[test]
    fn a_topic_the_inventory_cannot_answer_is_reported_not_hidden() {
        let (families, urls) = tutorial();
        let plan = build_with_inventory(&families, &urls, Some("channels quantum"), 4);
        assert_eq!(plan.coverage.focus_topics, 2);
        assert_eq!(plan.coverage.focus_topics_answered, 1);
        assert_eq!(plan.coverage.focus_unanswered, vec!["quantum".to_string()]);
    }

    #[test]
    fn a_six_page_multi_topic_plan_covers_all_three_named_pages() {
        let (families, urls) = tutorial();
        let plan = build_with_inventory(&families, &urls, Some("channels select shared state"), 6);
        assert_eq!(plan.pages.len(), 3);
        let planned: HashSet<&str> = plan.pages.iter().map(|p| p.url.as_str()).collect();
        assert!(planned.iter().any(|u| u.ends_with("shared-state")));
        assert!(planned.iter().any(|u| u.ends_with("/channels")));
        assert!(planned.iter().any(|u| u.ends_with("/select")));
        assert_eq!(plan.coverage.focus_topics_answered, 4);
        assert_eq!(plan.stop, PlanStop::Covered);
    }
}
