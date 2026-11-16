//! The crawl site model: the map's entry point and the summary the tool,
//! the planner and the dossier share.
//!
//! A map answers "what is this site?" without reading a content page: the
//! inventory, its templates and families, its section tree and its change
//! hash. A repeat map inside [`MAP_TTL_SECS`] with the same seed and options
//! serves the dossier, so the CLI's warm map is a file read. The old
//! frontier walker owns `full`/`content` until M6; this module is the v6
//! model, not a second crawler.

pub mod dossier;
pub mod infer;
pub mod inventory;
pub mod plan;
#[cfg(test)]
mod tests;

use crate::crawl::frontier;
use crate::crawl::sitemap::Robots;
use crate::crawl::{
    CrawlOptions, FetchedPage, InventoryStatus, PageFetcher, SeedFailure, StopReason,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::time::{Instant, SystemTime, UNIX_EPOCH};
use url::Url;

/// A warmed map is the same model for five minutes unless the seed, focus,
/// scope or budget changed. M5 replaces this TTL with a change contract.
pub const MAP_TTL_SECS: u64 = 300;

/// The previous map of this host, for the changed/unchanged line.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PreviousMap {
    pub generated_at: u64,
    pub changed: bool,
}

/// The model summary a map returns and a dossier stores.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ModelSummary {
    pub host: String,
    pub seed: String,
    pub site_kind: infer::SiteKind,
    pub urls: usize,
    /// URLs the inventory read before scope, locale and policy filters.
    pub raw: usize,
    pub templates: usize,
    pub families: usize,
    pub sections: usize,
    /// Inventory reads this map made (robots is the caller's preflight).
    pub requests: usize,
    pub complete: bool,
    /// The primary inventory source: sitemap, pagelist, llms or seed links.
    pub source: String,
    /// True when this response came from the dossier, not the network.
    #[serde(default)]
    pub cached: bool,
    pub sources: BTreeMap<String, usize>,
    pub filtered: infer::FilterCounts,
    pub inventory_hash: String,
    pub generated_at: u64,
    pub previous: Option<PreviousMap>,
    pub dossier_saved: bool,
    /// Agent interfaces the site declared in `llms.txt`.
    pub apis: BTreeMap<String, String>,
    pub top_sections: Vec<infer::Section>,
    pub top_families: Vec<infer::Family>,
}

pub struct MapRequest<'a> {
    /// The seed after the caller's redirect resolution.
    pub seed: &'a Url,
    /// The seed as the caller gave it, for a failure report.
    pub requested_seed: &'a str,
    /// The seed page when the caller already has it. None lets the
    /// fallback fetch it once.
    pub seed_page: Option<&'a FetchedPage>,
    pub robots: Option<&'a Robots>,
    pub opts: &'a CrawlOptions,
    pub deadline: Instant,
    /// A failure the caller's preflight already established (robots).
    pub seed_failure: Option<SeedFailure>,
}

pub struct MapOutcome {
    pub map: Vec<String>,
    pub inventory: InventoryStatus,
    pub summary: ModelSummary,
    /// Every family the map built; the planner's input.
    pub families: Vec<infer::Family>,
    /// The section tree, rebuilt from the families for a warmed dossier.
    pub sections: Vec<infer::Section>,
    /// The full in-scope inventory. Focus plans need every URL, not the
    /// capped map view; a warm map (no focus) leaves this empty.
    pub urls: Vec<infer::InvUrl>,
    pub seed_failure: Option<SeedFailure>,
    pub filtered_out: usize,
    pub stop: StopReason,
}

/// Build the map for one seed. Never fetches a content page: the inventory
/// sources are robots (caller), `llms.txt`, sitemaps, feeds and, only when
/// none of those answered, the seed's own links.
pub async fn map(fetch: &PageFetcher, req: MapRequest<'_>) -> MapOutcome {
    // The seed's own section is the scope unless the caller narrowed it.
    let mut include = req.opts.include_paths.clone();
    if include.is_empty()
        && let Some(scope) = frontier::auto_scope(req.seed.path())
    {
        include = vec![scope];
    }
    let exclude = frontier::effective_excludes(&req.opts.exclude_paths);
    let seed_query = req.opts.focus.clone().or_else(|| {
        req.seed
            .path()
            .rsplit('/')
            .find(|s| !s.is_empty())
            .map(|s| s.replace(['-', '_', '.'], " "))
    });
    let host = req.seed.host_str().unwrap_or("").to_ascii_lowercase();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let previous = dossier::load(&host);

    // A warm map: the dossier is the same model for the same request. The
    // flat URL view is stored beside it, so the CLI's second call does no
    // network I/O at all.
    if let Some(d) = previous.as_ref().filter(|d| {
        d.summary.generated_at.saturating_add(MAP_TTL_SECS) >= now
            && d.seed == req.seed.to_string()
            && d.focus.as_deref() == req.opts.focus.as_deref()
            && d.include == include
            && d.exclude == exclude
            && d.map_cap == req.opts.map_cap
            && d.same_host == req.opts.same_host
            && d.respect_robots == req.opts.respect_robots
            && !d.map.is_empty()
            // A focused plan needs the full inventory: the map view is
            // capped and ranked, so a warm map would under-cover.
            && !(req.opts.mode == crate::crawl::CrawlMode::Plan && req.opts.focus.is_some())
            && !req.opts.force_fresh_map
    }) {
        let mut summary = d.summary.clone();
        summary.cached = true;
        summary.requests = 0;
        summary.previous = Some(PreviousMap {
            generated_at: d.summary.generated_at,
            changed: false,
        });
        let source = static_source(&summary.source);
        let filtered_out = summary.filtered.total();
        let stop = if summary.complete {
            StopReason::FrontierEmpty
        } else if d.map.len() >= req.opts.map_cap {
            StopReason::InventoryLimit
        } else {
            StopReason::FrontierEmpty
        };
        return MapOutcome {
            map: d.map.clone(),
            inventory: InventoryStatus {
                complete: summary.complete,
                source,
                limit: req.opts.map_cap,
            },
            families: d.families.clone(),
            sections: infer::sections_from_families(&d.families),
            urls: Vec::new(),
            summary,
            seed_failure: None,
            filtered_out,
            stop,
        };
    }

    let inv = inventory::collect(
        fetch,
        &inventory::InventoryOptions {
            seed: req.seed,
            requested_seed: req.requested_seed,
            seed_page: req.seed_page,
            robots: req.robots,
            focus: req.opts.focus.as_deref(),
            seed_query: seed_query.as_deref(),
            include: &include,
            exclude: &exclude,
            same_host: req.opts.same_host,
            respect_robots: req.opts.respect_robots,
            deadline: req.deadline,
            request_budget: inventory::INVENTORY_REQUEST_BUDGET,
            url_cap: inventory::URL_CAP,
            map_cap: req.opts.map_cap,
        },
    )
    .await;

    let built = infer::build(&inv.urls, req.seed);
    let changed = previous
        .as_ref()
        .is_some_and(|p| p.summary.inventory_hash != built.inventory_hash);
    let mut summary = ModelSummary {
        host: host.clone(),
        seed: req.seed.to_string(),
        site_kind: built.site_kind,
        urls: built.urls,
        raw: inv.raw,
        templates: built.templates,
        families: built.families.len(),
        sections: built.sections.len(),
        requests: inv.requests,
        complete: inv.complete,
        source: inv.source.to_string(),
        cached: false,
        sources: inv.sources.clone(),
        filtered: inv.filtered.clone(),
        inventory_hash: built.inventory_hash.clone(),
        generated_at: now,
        previous: previous.as_ref().map(|p| PreviousMap {
            generated_at: p.summary.generated_at,
            changed,
        }),
        dossier_saved: true,
        apis: inv.apis.clone(),
        top_sections: built.sections.iter().take(40).cloned().collect(),
        top_families: built.families.iter().take(40).cloned().collect(),
    };
    let inventory_urls: Vec<String> = inv.urls.iter().map(|u| u.url.clone()).collect();
    let contract = dossier::ChangeContract {
        inventory_hash: built.inventory_hash.clone(),
        source: inv.source.to_string(),
        sitemap_lastmod_trust: inv.urls.iter().any(|u| u.lastmod.is_some()),
        page_list_stable: inv.complete && matches!(inv.source, "sitemap" | "pagelist"),
        checked_at: now,
    };
    let saved = dossier::save(
        &host,
        &dossier::Snapshot {
            summary: &summary,
            families: &built.families,
            map: &inv.map,
            inventory: &inventory_urls,
            contract: Some(&contract),
            focus: req.opts.focus.as_deref(),
            include: &include,
            exclude: &exclude,
            map_cap: req.opts.map_cap,
            same_host: req.opts.same_host,
            respect_robots: req.opts.respect_robots,
        },
    );
    summary.dossier_saved = saved;

    let map_empty = inv.map.is_empty();
    let stop = if inv.deadline_hit {
        StopReason::Deadline
    } else if !map_empty && !inv.complete && inv.map.len() >= req.opts.map_cap {
        StopReason::InventoryLimit
    } else {
        StopReason::FrontierEmpty
    };
    let mut inv = inv;
    let seed_failure = if map_empty && stop == StopReason::FrontierEmpty {
        req.seed_failure.or(inv.seed_failure.take())
    } else {
        None
    };
    MapOutcome {
        map: inv.map,
        inventory: InventoryStatus {
            complete: inv.complete,
            source: inv.source,
            limit: req.opts.map_cap,
        },
        families: built.families,
        sections: built.sections,
        urls: inv.urls,
        summary,
        seed_failure,
        filtered_out: inv.filtered.total(),
        stop,
    }
}

fn static_source(source: &str) -> &'static str {
    match source {
        "pagelist" => "pagelist",
        "sitemap" => "sitemap",
        "llms" => "llms",
        _ => "seed-links",
    }
}
