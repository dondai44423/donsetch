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
//! Providers: tavily, exa, serper, serpapi, serpbase, bravesearch, tinyfish, parallel, brightdata, unlocker

use super::{bold, dim, green, red};

use crate::DISPLAY_NAME;
use crate::search::byok::plugin::{
    DEFAULT_TIMEOUT_MS, PluginConfig, tokenize_cmd, validate_plugin_name,
};
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

pub async fn run(args: &[String]) {
    let sub = args.get(2).map(|s| s.as_str()).unwrap_or("help");

    match sub {
        "add" => cmd_add(args).await,
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

/// `donsetch keys add plugin <name> --cmd '...' [--timeout N] [--test]`
async fn cmd_add_plugin(args: &[String]) {
    let name = match args.get(4) {
        Some(n) if !n.trim().is_empty() => n.trim().to_ascii_lowercase(),
        _ => {
            eprintln!(
                "{} usage: donsetch keys add plugin <name> --cmd 'program [args...]' [--timeout <sec>] [--test]",
                red("\u{2717}")
            );
            eprintln!(
                "   the plugin is any executable that answers queries over stdin/stdout JSON (see README)"
            );
            std::process::exit(1);
        }
    };

    let mut cmd_str: Option<String> = None;
    let mut timeout_ms: Option<u64> = None;
    let mut test = false;
    let rest = &args[5..];
    let mut i = 0;
    while i < rest.len() {
        let a = rest[i].as_str();
        match a {
            "--cmd" | "-c" => {
                i += 1;
                let Some(v) = rest.get(i) else {
                    eprintln!("{} --cmd needs a command string", red("\u{2717}"));
                    std::process::exit(1);
                };
                if v.trim().is_empty() {
                    eprintln!("{} --cmd needs a command string", red("\u{2717}"));
                    std::process::exit(1);
                }
                cmd_str = Some(v.clone());
            }
            "--timeout" | "-t" => {
                i += 1;
                let v = match rest.get(i).and_then(|v| v.parse::<u64>().ok()) {
                    Some(v) if v > 0 => v,
                    _ => {
                        eprintln!(
                            "{} --timeout needs a positive whole number of seconds",
                            red("\u{2717}")
                        );
                        std::process::exit(1);
                    }
                };
                timeout_ms = v.checked_mul(1000);
            }
            "--test" => test = true,
            other if other.starts_with("--cmd=") => {
                cmd_str = Some(other.trim_start_matches("--cmd=").to_string());
            }
            other if other.starts_with("--timeout=") => {
                let v = other
                    .trim_start_matches("--timeout=")
                    .parse::<u64>()
                    .unwrap_or(0);
                if v == 0 {
                    eprintln!(
                        "{} --timeout needs a positive whole number of seconds",
                        red("\u{2717}")
                    );
                    std::process::exit(1);
                }
                timeout_ms = v.checked_mul(1000);
            }
            other => {
                eprintln!("{} unknown flag: {other}", red("\u{2717}"));
                eprintln!(
                    "   usage: donsetch keys add plugin <name> --cmd 'program [args...]' [--timeout <sec>] [--test]"
                );
                std::process::exit(1);
            }
        }
        i += 1;
    }

    let cmd_str = match cmd_str {
        Some(c) if !c.trim().is_empty() => c,
        _ => {
            eprintln!(
                "{} --cmd is required: donsetch keys add plugin {name} --cmd 'program [args...]'",
                red("\u{2717}")
            );
            std::process::exit(1);
        }
    };
    let cmd = match tokenize_cmd(&cmd_str) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{} bad --cmd value: {e}", red("\u{2717}"));
            std::process::exit(1);
        }
    };

    let byok_cfg = ByokConfig::load();
    let keyed: std::collections::HashSet<String> =
        byok_cfg.providers.iter().map(|p| p.name.clone()).collect();
    if let Err(e) = validate_plugin_name(&name, &keyed) {
        eprintln!("  {} {e}", red("\u{2717}"));
        std::process::exit(1);
    }

    let mut cfg = PluginConfig::load();
    if let Err(e) = cfg.add(
        &name,
        cmd.clone(),
        timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS),
        &keyed,
    ) {
        eprintln!("  {} {e}", red("\u{2717}"));
        std::process::exit(1);
    }
    cfg.save();

    // First backend ever sets the default, unless the user
    // explicitly pinned "local" before adding anything.
    let mut byok_cfg = byok_cfg.clone();
    let became_default = if byok_cfg.default.is_empty() {
        byok_cfg.default = name.clone();
        byok_cfg.save();
        true
    } else {
        byok_cfg.default == name
    };

    println!(
        "  {} registered plugin {} (timeout {}s)",
        green("\u{2713}"),
        bold(&name),
        cfg.plugins[&name].timeout_ms / 1000
    );
    println!("  {} program: {}", dim("      "), dim(&cmd.join(" ")));
    if became_default {
        println!(
            "  {} {} is now the default search provider.",
            dim("note:"),
            bold(&name)
        );
    } else {
        println!(
            "  {} default stays {}: use {} to switch.",
            dim("note:"),
            bold(if byok_cfg.default.is_empty() {
                "local"
            } else {
                &byok_cfg.default
            }),
            bold(&format!("donsetch keys default {name}"))
        );
    }

    if test {
        println!();
        println!(
            "  {} running one probe query through {} (this calls your adapter once)...",
            dim("      "),
            bold(&name)
        );
        let def = cfg.plugins[&name].clone();
        match crate::search::byok::plugin::probe(&name, &def).await {
            Ok(n) => {
                println!("  {} probe ok: {n} result(s)", green("\u{2713}"));
            }
            Err(e) => {
                eprintln!("  {} probe failed: {e}", red("\u{2717}"));
                std::process::exit(1);
            }
        }
    } else {
        println!(
            "  {} verify it end to end with {}.",
            dim("tip:"),
            bold(&format!("donsetch keys add plugin {name} --cmd ... --test"))
        );
    }
    println!(
        "  {} plugins run at search time: restart your MCP server if running.",
        dim("      ")
    );
}

async fn cmd_add(args: &[String]) {
    if args.get(3).map(String::as_str) == Some("plugin") {
        cmd_add_plugin(args).await;
        return;
    }
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

    // Paid-integration keys deserve instant shape feedback:
    // a malformed key is caught here, not as a confusing API
    // rejection on the first paid request.
    if provider == "unlocker"
        && let Err(e) = crate::fetch::bypass::parse_key(&key, crate::fetch::bypass::DEFAULT_ZONE)
    {
        eprintln!("  {} {e}", red("\u{2717}"));
        std::process::exit(1);
    }
    if provider == "brightdata"
        && let Err(e) = crate::search::byok::brightdata_key_parts(&key)
    {
        eprintln!("  {} {e}", red("\u{2717}"));
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
            "  {} added key to {} (stacked : {} providers now configured)",
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
                "  {} keys configured : local is default, BYOK is fallback.",
                dim("note:")
            );
        } else {
            println!(
                "  {} BYOK search is now active : local search is bypassed.",
                dim("note:")
            );
        }
        println!("  {} Restart your MCP server if running.", dim("      "));
    }

    // Unlocker keys: confirm the zone and point at the free
    // validation so the first wall hit does not surprise anyone.
    if provider == "unlocker" {
        let (_, zone) = crate::fetch::bypass::parse_key(&key, crate::fetch::bypass::DEFAULT_ZONE)
            .expect("validated above");
        println!();
        println!(
            "  {} unlocker ready: zone {}, solve-cache on (repeat walls never bill twice)",
            green("\u{2713}"),
            bold(&zone)
        );
        println!(
            "  {} {} validates the token and zone for free before any paid unlock",
            dim("tip:"),
            green("donsetch doctor --deep")
        );
    }
}

fn cmd_remove(args: &[String]) {
    let provider = match args.get(3) {
        Some(p) if p == "plugin" => {
            cmd_remove_plugin(args);
            return;
        }
        Some(p) => parse_provider(p),
        None => {
            eprintln!(
                "{} usage: donsetch keys remove <provider> [key]   |   donsetch keys remove plugin <name>",
                red("\u{2717}")
            );
            eprintln!("   providers: {}", dim(&PROVIDERS.join(", ")));
            std::process::exit(1);
        }
    };

    // A non-native name may still resolve to a registered plugin:
    // `keys remove searxng` falls through to plugin removal.
    if !PROVIDERS.contains(&provider.as_str()) {
        let cfg = PluginConfig::load();
        if cfg.is_registered(&provider) {
            let fixed = vec![
                "keys".to_string(),
                "remove".to_string(),
                "plugin".to_string(),
                provider,
            ];
            cmd_remove_plugin(&fixed);
            return;
        }
    }

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
                "  {} removed all keys for {} : no providers remaining",
                green("\u{2713}"),
                bold(&provider)
            );
            println!(
                "  {} BYOK search disabled : local search is active.",
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

/// `donsetch keys remove plugin <name>`
fn cmd_remove_plugin(args: &[String]) {
    let name = match args.get(4) {
        Some(n) if !n.trim().is_empty() => n.trim().to_ascii_lowercase(),
        _ => {
            eprintln!(
                "{} usage: donsetch keys remove plugin <name>",
                red("\u{2717}")
            );
            std::process::exit(1);
        }
    };

    let mut cfg = PluginConfig::load();
    if !cfg.remove(&name) {
        eprintln!("  {} no plugin named {name}", red("\u{2717}"));
        std::process::exit(1);
    }
    cfg.save();

    // If the removed plugin was the default, hand the default to
    // the first remaining keyed provider, else the first plugin,
    // else clear it (local search).
    let mut byok_cfg = ByokConfig::load();
    if byok_cfg.default == name {
        byok_cfg.default = byok_cfg
            .providers
            .first()
            .map(|p| p.name.clone())
            .or_else(|| {
                let mut names = cfg.names();
                names.next().cloned()
            })
            .unwrap_or_default();
        byok_cfg.save();
    }

    println!("  {} removed plugin {}", green("\u{2713}"), bold(&name));
    if !byok_cfg.default.is_empty() {
        println!(
            "  {} default is now {}",
            dim("note:"),
            bold(&byok_cfg.default)
        );
    }
}

fn cmd_list() {
    let cfg = ByokConfig::load();
    render_list(&cfg);

    let pc = PluginConfig::load();
    if !pc.is_configured() {
        return;
    }

    println!("  {} {}", dim(""), bold("BYOK Search Plugins"));
    println!();
    for name in pc.names() {
        let def = &pc.plugins[name];
        let marker = if cfg.default == *name {
            green("\u{25C6}")
        } else {
            dim("\u{25C7}")
        };
        let label = if cfg.default == *name {
            format!("{} {} {}", marker, bold(name), dim("(default)"))
        } else {
            format!("{marker} {name}")
        };
        println!("  {label}");
        println!(
            "    {} {}  (timeout {}s)",
            green("\u{2713}"),
            dim(&def.cmd.join(" ")),
            def.timeout_ms / 1000
        );
        println!();
    }
    println!(
        "  {} plugins are user-registered executables : see README BYOK plugins",
        dim("")
    );
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

    // "local" bypasses the is_configured() check : you can set
    // local as default even with keys configured (to test local
    // search without removing keys), or with no keys (no-op).
    if provider != "local"
        && !cfg.is_configured()
        && !PROVIDERS.contains(&provider.as_str())
        && !PluginConfig::load().is_registered(&provider)
    {
        eprintln!("  {} no keys configured", red("\u{2717}"));
        std::process::exit(1);
    }

    if cfg.set_default(&provider) {
        cfg.save();
        if provider == "local" {
            if cfg.is_configured() {
                println!(
                    "  {} default set to {} : local search first, BYOK fallback",
                    green("\u{2713}"),
                    bold("local")
                );
            } else {
                println!(
                    "  {} default set to {} (no BYOK keys : local is already active)",
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
    } else if PluginConfig::load().is_registered(&provider) {
        // Plugin default: plugins live in a separate store, so
        // set the shared default field directly.
        cfg.default = provider.clone();
        cfg.save();
        println!(
            "  {} default search provider set to plugin {}",
            green("\u{2713}"),
            bold(&provider)
        );
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
            "  {} no keys in import : local search is active",
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
        bold("donsetch keys : BYOK search provider management")
    );
    println!();
    println!("  {}", bold("Commands:"));
    println!(
        "    {} <provider> <key>     Add a key (auto-sets default if first)",
        green("add")
    );
    println!(
        "    {} plugin <name>       Register a plugin executable (BYOK adapter)",
        green("add")
    );
    println!("        --cmd 'program [args...]'   the adapter command (any language)");
    println!(
        "    {} <provider> [key]    Remove a key (or all keys for a provider)",
        green("remove")
    );
    println!(
        "    {} plugin <name>       Remove a registered plugin",
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
    println!("    serpbase   SerpBase Google SERP (serpbase.dev)");
    println!("    bravesearch Brave Search API (api.search.brave.com)");
    println!("    tinyfish   TinyFish Search (api.search.tinyfish.ai)");
    println!("    parallel   Parallel AI Search (api.parallel.ai) : fast mode");
    println!("    brightdata Bright Data SERP API (api.brightdata.com)");
    println!(
        "    unlocker   Bright Data Web Unlocker (paid solver for walled sites; key:: zone, fetch-side)"
    );
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
    println!("    If all providers are exhausted, {DISPLAY_NAME} falls back");
    println!("    to the local keyless 5-engine search system.");
    println!();
    println!("  {}", bold("Plugins (for unsupported providers):"));
    println!("    If the platform you have a key for is not natively");
    println!("    supported, register any executable that answers our");
    println!("    stdin/stdout JSON contract as a plugin, with no code");
    println!("    changes to {DISPLAY_NAME}:");
    println!(
        "      {}",
        dim("donsetch keys add plugin myprovider --cmd 'python3 ~/adapter.py' --test")
    );
    println!("    Any language works: shell, python, a compiled binary.");
}
