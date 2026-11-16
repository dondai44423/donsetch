//! The site model: URL skeletons, families and the section tree.
//!
//! Pure: input is one inventory, output is the model the plan and the
//! dossier consume. No network and no state live here.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
use url::Url;

/// One inventory URL, already normalized by `frontier::normalize`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InvUrl {
    pub url: String,
    #[serde(default)]
    pub lastmod: Option<String>,
    #[serde(default)]
    pub source: String,
    /// The site's own label for the page: link text, an `llms.txt` title or
    /// a feed item title. A page's URL does not always carry its topic
    /// (`/tutorial/errors.html` is "Errors and Exceptions"), so recall
    /// scores this label beside the path.
    #[serde(default)]
    pub title: Option<String>,
}

/// One `(section, template)` family: the unit of coverage.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Family {
    pub section: String,
    pub template: String,
    pub size: usize,
    #[serde(default)]
    pub lastmod: Option<String>,
    #[serde(default)]
    pub sample: Vec<String>,
}

/// One section of the site's tree.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Section {
    pub path: String,
    pub urls: usize,
    pub families: usize,
    pub date: bool,
    #[serde(default)]
    pub lastmod: Option<String>,
    #[serde(default)]
    pub sample: Vec<String>,
}

/// What the shape of the site means for the plan's breadth/depth split.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SiteKind {
    #[default]
    Site,
    Docs,
    Blog,
    Catalog,
    App,
}

impl SiteKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Site => "site",
            Self::Docs => "docs",
            Self::Blog => "blog",
            Self::Catalog => "catalog",
            Self::App => "app",
        }
    }
}

/// Counts of inventory entries a filter removed. Every map reports them:
/// a silently tiny map is a bug.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FilterCounts {
    pub robots: usize,
    pub rules: usize,
    pub scope: usize,
    pub locale: usize,
    pub assets: usize,
    pub other: usize,
}

impl FilterCounts {
    pub fn total(&self) -> usize {
        self.robots + self.rules + self.scope + self.locale + self.assets + self.other
    }
}

/// The model built from one inventory.
#[derive(Debug, Clone, Default)]
pub struct Built {
    pub urls: usize,
    pub templates: usize,
    pub families: Vec<Family>,
    pub sections: Vec<Section>,
    pub site_kind: SiteKind,
    pub inventory_hash: String,
}

/// Build templates, families, sections and the site kind from one
/// deduplicated inventory. Deterministic: the same inventory and seed
/// produce the same counts and hash, so a repeat map is comparable.
pub fn build(urls: &[InvUrl], _seed: &Url) -> Built {
    let parsed: Vec<(String, Option<String>, Vec<String>)> = urls
        .iter()
        .filter_map(|u| {
            Url::parse(&u.url)
                .ok()
                .map(|p| (u.url.clone(), u.lastmod.clone(), segments(&p)))
        })
        .collect();
    if parsed.is_empty() {
        return Built::default();
    }

    // (parent prefix, segment) frequency decides structural vs slot.
    let mut freq: HashMap<(String, String), usize> = HashMap::new();
    let mut paths: Vec<Vec<String>> = Vec::with_capacity(parsed.len());
    for (_, _, segs) in &parsed {
        let mut parent = String::new();
        for seg in segs {
            *freq
                .entry((parent.clone(), seg.to_lowercase()))
                .or_default() += 1;
            parent.push('/');
            parent.push_str(&seg.to_lowercase());
        }
        paths.push(segs.clone());
    }
    let sec_depth = choose_section_depth(&paths);

    let mut fam_urls: HashMap<(String, String), Vec<String>> = HashMap::new();
    let mut fam_last: HashMap<(String, String), Option<String>> = HashMap::new();
    let mut templates: HashSet<String> = HashSet::new();
    for (url, lastmod, segs) in &parsed {
        let mut tpl: Vec<String> = Vec::new();
        let mut parent = String::new();
        for seg in segs {
            let structural = !variable(seg)
                && freq
                    .get(&(parent.clone(), seg.to_lowercase()))
                    .copied()
                    .unwrap_or(0)
                    >= 3;
            if structural {
                tpl.push(seg.clone());
                parent.push('/');
                parent.push_str(&seg.to_lowercase());
            } else {
                if tpl.last().map(String::as_str) != Some(":v") {
                    tpl.push(":v".to_string());
                }
                parent.push('/');
                parent.push_str(&seg.to_lowercase());
            }
        }
        let template = if tpl.is_empty() {
            ":v".to_string()
        } else {
            tpl.join("/")
        };
        let section = if segs.is_empty() {
            "root".to_string()
        } else {
            segs.iter()
                .take(sec_depth.min(segs.len()))
                .cloned()
                .collect::<Vec<_>>()
                .join("/")
        };
        templates.insert(template.clone());
        let key = (section.clone(), template.clone());
        fam_urls.entry(key.clone()).or_default().push(url.clone());
        let slot = fam_last.entry(key).or_insert(None);
        *slot = newer(slot.take(), lastmod.clone());
    }

    // A family is one (section, template): its URLs, newest stamp, samples.
    let mut families: Vec<Family> = Vec::with_capacity(fam_urls.len());
    for (key, mut us) in fam_urls {
        us.sort();
        us.dedup();
        let sample: Vec<String> = us.iter().take(3).cloned().collect();
        families.push(Family {
            section: key.0.clone(),
            template: key.1.clone(),
            size: us.len(),
            lastmod: fam_last.remove(&key).flatten(),
            sample,
        });
    }
    families.sort_by(|a, b| {
        b.size
            .cmp(&a.size)
            .then_with(|| a.section.cmp(&b.section))
            .then_with(|| a.template.cmp(&b.template))
    });

    let sections = sections_from_families(&families);

    let date_sections = sections.iter().filter(|s| s.date).count();
    let site_kind =
        if date_sections >= 3 || (date_sections >= 2 && date_sections * 2 >= sections.len()) {
            SiteKind::Blog
        } else if sections.len() >= 12 {
            SiteKind::Docs
        } else if families.len() >= 4 && sections.len() <= 3 && urls.len() / families.len() >= 8 {
            SiteKind::Catalog
        } else if families.len() <= 6 && sections.len() <= 4 {
            SiteKind::App
        } else {
            SiteKind::Site
        };

    let mut hash_input: Vec<String> = urls
        .iter()
        .map(|u| format!("{}\t{}", u.url, u.lastmod.as_deref().unwrap_or("")))
        .collect();
    hash_input.sort();
    let mut h = FNV_OFFSET;
    for line in hash_input {
        for b in line.as_bytes() {
            h ^= *b as u64;
            h = h.wrapping_mul(FNV_PRIME);
        }
        h ^= b'\n' as u64;
        h = h.wrapping_mul(FNV_PRIME);
    }

    Built {
        urls: urls.len(),
        templates: templates.len(),
        families,
        sections,
        site_kind,
        inventory_hash: format!("{h:016x}"),
    }
}

/// Aggregate `(section, template)` families into the section tree. Public
/// because a warmed dossier stores families, not sections; the planner
/// rebuilds the same tree from them.
pub fn sections_from_families(families: &[Family]) -> Vec<Section> {
    let mut section_map: BTreeMap<String, Section> = BTreeMap::new();
    for f in families {
        let entry = section_map
            .entry(f.section.clone())
            .or_insert_with(|| Section {
                path: f.section.clone(),
                date: date_section(&f.section),
                ..Section::default()
            });
        entry.urls += f.size;
        entry.families += 1;
        entry.lastmod = newer(entry.lastmod.take(), f.lastmod.clone());
        for s in &f.sample {
            if entry.sample.len() < 3 && !entry.sample.contains(s) {
                entry.sample.push(s.clone());
            }
        }
    }
    let mut sections: Vec<Section> = section_map.into_values().collect();
    sections.sort_by(|a, b| b.urls.cmp(&a.urls).then_with(|| a.path.cmp(&b.path)));
    sections
}

const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

fn segments(u: &Url) -> Vec<String> {
    u.path()
        .split('/')
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

/// The section depth is the shallowest depth whose prefixes form a real
/// tree: many recurring branches, no one family holding the site. The
/// shallowest such depth keeps a docs site on its product sections
/// instead of one page per element; a date archive lands on its years.
fn choose_section_depth(parsed: &[Vec<String>]) -> usize {
    let mut best = (0usize, 1usize);
    for d in 1..=8 {
        let mut counts: HashMap<String, usize> = HashMap::new();
        for segs in parsed {
            if segs.len() >= d {
                *counts
                    .entry(segs[..d].join("/").to_lowercase())
                    .or_default() += 1;
            }
        }
        let total: usize = counts.values().sum();
        if total == 0 {
            break;
        }
        let recurring = counts.values().filter(|n| **n >= 2).count();
        let top = counts.values().copied().max().unwrap_or(0) as f64 / total as f64;
        if recurring >= 8 && top < 0.75 {
            return d;
        }
        if recurring > best.0 {
            best = (recurring, d);
        }
    }
    best.1
}

/// A segment a site generates, not a structural word: dates, ids, page
/// numbers, index files. These become `:v` slots.
fn variable(seg: &str) -> bool {
    let l = seg.to_lowercase();
    if l.is_empty() {
        return false;
    }
    if matches!(
        l.as_str(),
        "jan"
            | "january"
            | "feb"
            | "february"
            | "mar"
            | "march"
            | "apr"
            | "april"
            | "may"
            | "jun"
            | "june"
            | "jul"
            | "july"
            | "aug"
            | "august"
            | "sep"
            | "september"
            | "oct"
            | "october"
            | "nov"
            | "november"
            | "dec"
            | "december"
    ) {
        return true;
    }
    if l == "index.html" || l == "index.htm" || l == "index.php" || l == "default.html" {
        return true;
    }
    if l.starts_with("page-") || l.starts_with("page_") {
        return l
            .trim_start_matches("page-")
            .trim_start_matches("page_")
            .chars()
            .all(|c| c.is_ascii_digit());
    }
    if l.len() >= 8 && l.chars().all(|c| c.is_ascii_hexdigit()) {
        return true;
    }
    if l.chars().all(|c| c.is_ascii_digit()) {
        return true;
    }
    l.len() == 4
        && (l.starts_with("19") || l.starts_with("20"))
        && l[2..].chars().all(|c| c.is_ascii_digit())
}

fn date_section(path: &str) -> bool {
    path.rsplit('/')
        .next()
        .is_some_and(|s| s.len() == 4 && (s.starts_with("19") || s.starts_with("20")))
}

fn newer(a: Option<String>, b: Option<String>) -> Option<String> {
    match (a, b) {
        (Some(a), Some(b)) => Some(if a >= b { a } else { b }),
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    }
}
