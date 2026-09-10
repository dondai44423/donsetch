//! `donsetch adapters`: the rewrite-adapter registry's human face.
//!
//! Lists the bundled rewrite adapters (via label + one-liner), then
//! the user plugin catalog under `cache_dir()/adapters/` with load
//! receipts. Read-only: never touches the network, never spawns
//! anything. The `<command> --help` text lives at the bottom.

use crate::adapters;
use crate::adapters::plugins;

pub fn run() {
    if std::env::var_os("DONSETCH_NO_ADAPTERS").is_some() {
        println!("Adapter registry is OFF (DONSETCH_NO_ADAPTERS is set).");
        println!("Everything below stays loaded in memory but never fires.");
        println!();
    }
    println!("Bundled rewrite adapters (first match wins, builtins before user plugins):");
    for b in adapters::builtins() {
        println!("  {:22} {}", b.via, b.description);
    }
    println!();

    let dir = crate::paths::cache_dir().join("adapters");
    let rows = plugins::catalog();
    let (_, skips) = plugins::catalog_stats();
    println!("User rewrite adapters: {}", dir.display());
    if rows.is_empty() && skips == 0 {
        println!("  (empty: drop *.json plugin files here, see `donsetch adapters --help`)");
    } else {
        for row in rows {
            let hosts = row.plugin.hosts.join(",");
            let prefix = row.plugin.path_prefix.as_deref().unwrap_or("/");
            println!(
                "  {:22} hosts={hosts} prefix={prefix} target={}",
                row.via, row.plugin.target
            );
        }
        if skips > 0 {
            println!("  skipped files: {skips} (see the [adapters] receipts above)");
        }
    }
    println!();
    println!("Extract adapters (bundled only in v1): github issues, stackexchange,");
    println!("wikipedia infoboxes, docs outlines, reddit json, package registries.");
}

/// Usage text for `donsetch adapters --help` and `help adapters`.
pub fn help() {
    println!("Usage: donsetch adapters");
    println!();
    println!("  Adapter registry: what rewrites on fetch, and what user plugins are");
    println!("  loaded. Where one exists, a page URL is rewritten to the site's own");
    println!("  public JSON endpoint before fetching (one cheap request, no JS shell).");
    println!();
    println!("User plugins: one JSON file per rewrite under cache_dir()/adapters/");
    println!("Example rules.json:");
    println!("{{\"name\": \"readthedocs-md\", \"hosts\": [\"cdn.rtd.io\"],");
    println!(" \"path_prefix\": \"/projects/\",");
    println!(" \"target\": \"https://cdn.rtd.io/api{{path}}.json\"}}");
    println!();
    println!("Rules:");
    println!("  name     slug [a-z0-9][a-z0-9-]* (1..40 chars; bundled names rejected)");
    println!("  hosts    1..=8 DNS hostnames, exact match, no ports/IPs");
    println!("  target   https only, no userinfo/port/query/fragment, one {{path}}");
    println!("           in the target's path; the input query string carries over");
    println!("  Behavior: only URL rewrites, pure data, no executables. The fetcher's");
    println!("  egress guards still apply to the rewritten URL.");
    println!();
    println!("Env:");
    println!("  DONSETCH_NO_ADAPTERS=1   Disable all adapters (bundled + user)");
    println!("  DONSETCH_ADAPTER_DUMP=D  Capture inspected bodies to D (debug)");
}
