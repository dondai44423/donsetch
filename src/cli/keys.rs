//! CLI: `donsetch keys <subcommand>`
//!
//!   donsetch keys add <provider> <key>     Add a key (auto-sets default if first)
//!   donsetch keys remove <provider> [key]  Remove a key (or all if no key)
//!   donsetch keys list                     Show all providers and key states
//!   donsetch keys default <provider|local> Set the default search method
//!   donsetch keys reset [provider]         Reset key states to active
//!   donsetch keys export [path|-]          Export keys to file or stdout
//!   donsetch keys import <path>            Import keys from a file
//!   donsetch keys clear                     Remove all keys
//!
//! Providers: tavily, exa, serper, serpapi, tinyfish, parallel, brightdata, unlocker

use super::{bold, dim, green, red};

use crate::search::byok::store::{ByokConfig, PROVIDERS, render_list};

/// Normalize provider aliases to canonical names.
/// `bd` -> `brightdata`
fn normalize_alias(provider: &str) -> String {
    match provider {
        "bd" => "brightdata".to_string(),
        "wu" | "brightdata-unlocker" => "unlocker".to_string(),
        _ => provider.to_string(),
    }
}

/// Parse and normalize a provider argument: lowercase, then
/// expand aliases like `bd` -> `brightdata`.
fn parse_provider(input: &str) -> String {
    normalize_alias(&input.to_lowercase())
}

pub fn run(args: &[String]) {
    let sub = args.get(2).map(|s| s.as_str()).unwrap_or("help");

    match sub {
        "add" => cmd_add(args),
        "remove" | "rm" => cmd_remove(args),
        "list" | "ls" => cmd_list(),
        "default" => cmd_default(args),
        "reset" => cmd_reset(args),
        "export" => cmd_export(args),
        "import" => cmd_import(args),
        "clear" => cmd_clear(),
        "help" | "-h" | "--help" => print_help(),
        _ => {
            eprintln!("{} unknown subcommand: {sub}", red("\u{2717}"));
            print_help();
            std::process::exit(1);
        }
    }
}

fn cmd_add(args: &[String]) {
    let provider = match args.get(3) {
        Some(p) => p,
        None => {
            eprintln!(
                "{} usage: donsetch keys add <provider> <key>",
                red("\u{2717}")
            );
            eprintln!("   providers: {}", dim(&PROVIDERS.join(", ")));
            std::process::exit(1);
        }
    };
    let key = match args.get(4) {
        Some(k) if !k.trim().is_empty() => k.trim().to_string(),
        _ => {
            eprintln!(
                "{} usage: donsetch keys add <provider> <key>",
                red("\u{2717}")
            );
            std::process::exit(1);
        }
    };

    let provider = parse_provider(provider);

    if !PROVIDERS.contains(&provider.as_str()) {
        eprintln!(
            "{} unknown provider: {provider}\n   providers: {}",
            red("\u{2717}"),
            dim(&PROVIDERS.join(", "))
        );
        std::process::exit(1);
    }

    let mut cfg = ByokConfig::load();
    let is_new = !cfg.providers.iter().any(|p| p.name == *provider);
    let was_first = cfg.providers.is_empty();

    cfg.add_key(&provider, &key);
    cfg.save();

    if was_first {
        if cfg.default == *provider {
            println!(
                "  {} added key to {} (set as default)",
                green("\u{2713}"),
                bold(&provider)
            );
        } else {
            println!("  {} added key to {}", green("\u{2713}"), bold(&provider));
        }
    } else if is_new {
        println!(
            "  {} added key to {} (stacked — {} providers now configured)",
            green("\u{2713}"),
            bold(&provider),
            cfg.providers.len()
        );
    } else {
        println!(
            "  {} stacked key onto {} ({} keys total)",
            green("\u{2713}"),
            bold(&provider),
            cfg.providers
                .iter()
                .find(|p| p.name == *provider)
                .map(|p| p.keys.len())
                .unwrap_or(0)
        );
    }

    if cfg.providers.len() == 1 {
        println!();
        if cfg.is_local_default() {
            println!(
                "  {} keys configured — local is default, BYOK is fallback.",
                dim("note:")
            );
        } else {
            println!(
                "  {} BYOK search is now active — local search is bypassed.",
                dim("note:")
            );
        }
        println!("  {} Restart your MCP server if running.", dim("      "));
    }
}

fn cmd_remove(args: &[String]) {
    let provider = match args.get(3) {
        Some(p) => parse_provider(p),
        None => {
            eprintln!(
                "{} usage: donsetch keys remove <provider> [key]",
                red("\u{2717}")
            );
            eprintln!("   providers: {}", dim(&PROVIDERS.join(", ")));
            std::process::exit(1);
        }
    };

    let mut cfg = ByokConfig::load();
    let key = args.get(4).map(|s| s.trim());

    let removed = cfg.remove_keys(&provider, key);
    if !removed {
        eprintln!("  {} no matching key found for {provider}", red("\u{2717}"));
        std::process::exit(1);
    }

    cfg.save();

    let remaining = cfg
        .providers
        .iter()
        .find(|p| p.name == *provider)
        .map(|p| p.keys.len())
        .unwrap_or(0);

    if remaining == 0 {
        if cfg.is_configured() {
            println!(
                "  {} removed all keys for {} (provider removed, default={})",
                green("\u{2713}"),
                bold(&provider),
                green(&cfg.default)
            );
        } else {
            println!(
                "  {} removed all keys for {} — no providers remaining",
                green("\u{2713}"),
                bold(&provider)
            );
            println!(
                "  {} BYOK search disabled — local search is active.",
                dim("note:")
            );
        }
    } else {
        println!(
            "  {} removed key from {} ({} keys remaining)",
            green("\u{2713}"),
            bold(&provider),
            remaining
        );
    }
}

fn cmd_list() {
    let cfg = ByokConfig::load();
    render_list(&cfg);
}

fn cmd_default(args: &[String]) {
    let provider = match args.get(3) {
        Some(p) => parse_provider(p),
        None => {
            eprintln!(
                "{} usage: donsetch keys default <provider|local>",
                red("\u{2717}")
            );
            eprintln!(
                "   providers: {} or {}",
                dim(&PROVIDERS.join(", ")),
                green("local")
            );
            std::process::exit(1);
        }
    };

    let mut cfg = ByokConfig::load();

    // "local" bypasses the is_configured() check — you can set
    // local as default even with keys configured (to test local
    // search without removing keys), or with no keys (no-op).
    if provider != "local" && !cfg.is_configured() {
        eprintln!("  {} no keys configured", red("\u{2717}"));
        std::process::exit(1);
    }

    if cfg.set_default(&provider) {
        cfg.save();
        if provider == "local" {
            if cfg.is_configured() {
                println!(
                    "  {} default set to {} — local search first, BYOK fallback",
                    green("\u{2713}"),
                    bold("local")
                );
            } else {
                println!(
                    "  {} default set to {} (no BYOK keys — local is already active)",
                    green("\u{2713}"),
                    bold("local")
                );
            }
        } else {
            println!(
                "  {} default provider set to {}",
                green("\u{2713}"),
                bold(&provider)
            );
        }
    } else {
        eprintln!(
            "  {} provider {provider} has no keys configured",
            red("\u{2717}")
        );
        std::process::exit(1);
    }
}

fn cmd_reset(args: &[String]) {
    let provider = args.get(3).map(|s| parse_provider(s));

    let mut cfg = ByokConfig::load();
    if !cfg.is_configured() {
        eprintln!("  {} no keys configured", red("\u{2717}"));
        std::process::exit(1);
    }

    // Check provider has keys if specified.
    if let Some(p) = &provider
        && !cfg.providers.iter().any(|pc| &pc.name == p)
    {
        eprintln!("  {} provider {p} has no keys configured", red("\u{2717}"));
        std::process::exit(1);
    }

    cfg.reset_states(provider.as_deref());
    cfg.save();

    match &provider {
        Some(p) => println!(
            "  {} reset all key states for {} to active",
            green("\u{2713}"),
            bold(p)
        ),
        None => println!(
            "  {} reset all key states to active ({} providers)",
            green("\u{2713}"),
            cfg.providers.len()
        ),
    }
}

fn cmd_export(args: &[String]) {
    let cfg = ByokConfig::load();
    let json = cfg.to_json();

    match args.get(3).map(|s| s.as_str()) {
        None | Some("-") => {
            // Write to stdout.
            print!("{json}");
        }
        Some(path) => {
            let p = std::path::Path::new(path);
            match std::fs::write(p, &json) {
                Ok(_) => {
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::PermissionsExt;
                        let _ = std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o600));
                    }
                    let n_providers = cfg.providers.len();
                    let n_keys: usize = cfg.providers.iter().map(|p| p.keys.len()).sum();
                    println!(
                        "  {} exported {n_providers} providers ({n_keys} keys) to {path}",
                        green("\u{2713}")
                    );
                    #[cfg(unix)]
                    println!(
                        "  {} file permissions set to 0600 (owner-only)",
                        dim("      ")
                    );
                }
                Err(e) => {
                    eprintln!("  {} failed to write {path}: {e}", red("\u{2717}"));
                    std::process::exit(1);
                }
            }
        }
    }
}

fn cmd_import(args: &[String]) {
    let path = match args.get(3) {
        Some(p) if !p.trim().is_empty() => p,
        _ => {
            eprintln!("{} usage: donsetch keys import <path>", red("\u{2717}"));
            eprintln!(
                "   import a config exported by {}",
                green("donsetch keys export")
            );
            std::process::exit(1);
        }
    };

    let json = match std::fs::read_to_string(path) {
        Ok(j) => j,
        Err(e) => {
            eprintln!("  {} failed to read {path}: {e}", red("\u{2717}"));
            std::process::exit(1);
        }
    };

    let cfg = match ByokConfig::from_json(&json) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("  {} invalid config file: {e}", red("\u{2717}"));
            std::process::exit(1);
        }
    };

    let n_providers = cfg.providers.len();
    let n_keys: usize = cfg.providers.iter().map(|p| p.keys.len()).sum();
    cfg.save();

    println!(
        "  {} imported {n_providers} providers ({n_keys} keys) from {path}",
        green("\u{2713}")
    );
    if n_providers > 0 {
        if cfg.is_local_default() {
            println!(
                "  {} default: local (local-first, BYOK fallback)",
                dim("      ")
            );
        } else if !cfg.default.is_empty() {
            println!(
                "  {} default: {} (BYOK-first, local fallback)",
                dim("      "),
                bold(&cfg.default)
            );
        }
        println!(
            "  {} previous keys were replaced. Restart your MCP server if running.",
            dim("note:")
        );
    } else {
        println!(
            "  {} no keys in import — local search is active",
            dim("note:")
        );
    }
}

fn cmd_clear() {
    let cfg = ByokConfig::load();
    if !cfg.is_configured() {
        println!("  {} no keys configured", dim(""));
        return;
    }
    let n_providers = cfg.providers.len();
    let n_keys: usize = cfg.providers.iter().map(|p| p.keys.len()).sum();

    let empty = ByokConfig::empty();
    empty.save();

    println!(
        "  {} cleared {n_providers} providers ({n_keys} keys)",
        green("\u{2713}")
    );
    println!(
        "  {} local search is now active. Restart your MCP server if running.",
        dim("note:")
    );
}

fn print_help() {
    println!(
        "{}",
        bold("donsetch keys — BYOK search provider management")
    );
    println!();
    println!("  {}", bold("Commands:"));
    println!(
        "    {} <provider> <key>     Add a key (auto-sets default if first)",
        green("add")
    );
    println!(
        "    {} <provider> [key]    Remove a key (or all keys for a provider)",
        green("remove")
    );
    println!(
        "    {}                     Show all providers, keys, and states",
        green("list")
    );
    println!(
        "    {} <provider|local>  Set the default search method",
        green("default")
    );
    println!(
        "    {} [provider]         Reset key states to active (fixes rate-limited/dead keys)",
        green("reset")
    );
    println!(
        "    {} [path|-]          Export keys to file (or stdout with -)",
        green("export")
    );
    println!(
        "    {} <path>            Import keys from a file (replaces current)",
        green("import")
    );
    println!(
        "    {}                     Remove all keys (full reset)",
        green("clear")
    );
    println!();
    println!("  {}", bold("Providers:"));
    println!("    tavily     Tavily Search API (api.tavily.com)");
    println!("    exa        Exa AI Search (api.exa.ai)");
    println!("    serper     Serper.dev Google SERP (google.serper.dev)");
    println!("    serpapi    SerpApi Google SERP (serpapi.com)");
    println!("    tinyfish   TinyFish Search (api.search.tinyfish.ai)");
    println!("    parallel   Parallel AI Search (api.parallel.ai) — fast mode");
    println!("    brightdata Bright Data SERP API (api.brightdata.com)");
    println!("    {}", dim("    aliases: bd = brightdata"));
    println!();
    println!("  {}", bold("Default:"));
    println!("    Set a provider as default → BYOK is tried first, local is fallback.");
    println!(
        "    Set {} as default → local is tried first, BYOK is fallback.",
        green("local")
    );
    println!("    Lets you test local search without removing your keys.");
    println!();
    println!("  {}", bold("Stacking:"));
    println!("    Add multiple keys to the same provider for rotation.");
    println!("    If one key hits a rate limit or runs out of credits,");
    println!("    the next key is tried automatically.");
    println!();
    println!("  {}", bold("Fallback:"));
    println!("    If all providers are exhausted, DonSeTch falls back");
    println!("    to the local keyless 5-engine search system.");
}
