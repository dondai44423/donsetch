//! `donsetch adapters`: the rewrite-adapter registry's human face.
//!
//! Lists the bundled rewrite adapters (via label + one-liner), then
//! the user plugin catalog under `cache_dir()/adapters/` with load
//! receipts. Read-only: never touches the network, never spawns
//! anything. The `<command> --help` text lives at the bottom.

use crate::DISPLAY_NAME;
use crate::adapters;
use crate::adapters::plugins;
use crate::cli;

pub fn run(args: &[String]) {
    crate::cli::args::Spec::new("adapters", "donsetch adapters", &[]).reject(&args[2..]);
    cli::init();
    cli::print_header(
        &format!("{DISPLAY_NAME} Adapters"),
        "rewrite & extraction registry",
    );

    if !crate::config::cfg().fetch.adapters {
        println!();
        cli::check_warn(
            "adapter registry OFF",
            "fetch.adapters = false (loaded in memory, never fires)",
        );
    }

    cli::print_section("Bundled Rewrite Adapters (first match wins)");
    for b in adapters::builtins() {
        let padded = format!("{:<22}", b.via);
        println!("  {} {}", cli::cyan(&padded), cli::dim(b.description));
    }

    let dir = crate::paths::cache_dir().join("adapters");
    let rows = plugins::catalog();
    let (_, skips) = plugins::catalog_stats();
    cli::print_section("User Rewrite Plugins");
    cli::print_kv("directory", &cli::dim(&dir.display().to_string()));
    if rows.is_empty() && skips == 0 {
        println!(
            "  {}",
            cli::dim("(empty · drop *.json plugin files here; see `donsetch adapters --help`)")
        );
    } else {
        for row in rows {
            let hosts = row.plugin.hosts.join(",");
            let prefix = row.plugin.path_prefix.as_deref().unwrap_or("/");
            let padded = format!("{:<22}", row.via);
            println!(
                "  {} {}",
                cli::cyan(&padded),
                cli::dim(&format!(
                    "hosts={hosts} prefix={prefix} target={}",
                    row.plugin.target
                ))
            );
        }
        if skips > 0 {
            cli::check_warn(
                "skipped plugins",
                &format!("{skips} invalid file(s) ignored (see [adapters] receipts above)"),
            );
        }
    }

    cli::print_section("Bundled Extract Adapters");
    println!(
        "  {}",
        cli::dim(
            "youtube (video cards & transcripts) · github issues · stackexchange · wikipedia infoboxes · docs outlines · reddit json · package registries"
        )
    );
    println!();
    cli::print_footer();
    cli::print_nav(&[("donsetch adapters --help", "plugin schema")]);
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
