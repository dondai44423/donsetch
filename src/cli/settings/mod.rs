//! `donsetch settings` — interactive Crimson-Noir Control Center TUI and
//! scriptable CLI commands (`list`, `get`, `set`, `reset`, `preset`, `path`).

pub mod schema;
pub mod term;
pub mod tui;

use std::io::IsTerminal;

use serde_json::json;

use crate::DISPLAY_NAME;
use crate::cli;
use crate::config::{self, DonsetchConfig, Loaded, TomlEditValue};

use schema::{PRESETS, SETTINGS, TabId};

pub async fn run(args: &[String]) {
    cli::init();
    let sub = args.get(2).map(String::as_str);

    match sub {
        None => run_interactive_or_summary().await,
        Some("help" | "-h" | "--help") => help(),
        Some("path") => {
            cli::args::Spec::new("settings path", "donsetch settings path", &[]).reject(&args[3..]);
            cmd_path();
        }
        Some("list" | "ls") => {
            cli::args::Spec::new(
                "settings list",
                "donsetch settings list [--section SECTION] [--modified] [--json]",
                &["--modified", "--json"],
            )
            .with_valued(&["--section"])
            .reject(&args[3..]);
            cmd_list(&args[3..]);
        }
        Some("get") => {
            cli::args::Spec::new(
                "settings get",
                "donsetch settings get <section.key> [--json]",
                &["--json"],
            )
            .reject(&args[3..]);
            cmd_get(&args[3..]);
        }
        Some("set") => {
            // Only validate leading flags before `<section.key>` so setting a
            // token or text value that begins with `-` is not misread as a flag.
            if let Some(first) = args.get(3)
                && first.starts_with('-')
            {
                cli::args::Spec::new(
                    "settings set",
                    "donsetch settings set <section.key> <value>",
                    &[],
                )
                .reject(&args[3..4]);
            }
            cmd_set(&args[3..]);
        }
        Some("reset") => {
            cli::args::Spec::new(
                "settings reset",
                "donsetch settings reset <section.key>",
                &[],
            )
            .reject(&args[3..]);
            cmd_reset(&args[3..]);
        }
        Some("preset") => {
            cli::args::Spec::new(
                "settings preset",
                "donsetch settings preset [<name>]",
                &["--json"],
            )
            .reject(&args[3..]);
            cmd_preset(&args[3..]);
        }
        Some(other) if other.starts_with('-') => {
            cli::args::Spec::new(
                "settings",
                "donsetch settings [list|get|set|reset|preset|path]",
                &[],
            )
            .reject(&args[2..]);
        }
        Some(other) => {
            const SUBS: &[&str] = &["list", "get", "set", "reset", "preset", "path"];
            crate::diag!("donsetch settings: unknown subcommand '{other}'");
            if let Some(tip) = cli::args::suggest(other, SUBS) {
                crate::diag!("  tip: a similar subcommand exists: '{tip}'");
            }
            crate::diag!("");
            help();
            std::process::exit(1);
        }
    }
}

fn load_or_exit() -> Loaded {
    match config::load_for_editor() {
        Ok(loaded) => {
            let _ = config::install_loaded(loaded.clone());
            loaded
        }
        Err(e) => {
            crate::diag!("donsetch settings: {e}");
            std::process::exit(1);
        }
    }
}

async fn run_interactive_or_summary() {
    let loaded = load_or_exit();
    if std::io::stdin().is_terminal() && std::io::stdout().is_terminal() {
        match tui::run(loaded).await {
            Ok(Some(summary)) => {
                println!("  {} {summary}", cli::icon_pass());
                println!(
                    "  {}",
                    cli::dim("Restart any running `donsetch mcp` server to apply file changes.")
                );
            }
            Ok(None) => {}
            Err(e) => {
                crate::diag!("{} {e}", cli::icon_fail());
                std::process::exit(1);
            }
        }
        return;
    }

    // Non-interactive fallback (pipes, scripts, AI agents): print a concise
    // overview and non-interactive commands instead of failing on raw mode.
    cli::print_header(
        &format!("{DISPLAY_NAME} Settings"),
        "non-interactive summary (run in a TTY for the full interactive UI)",
    );
    println!();
    match config::config_file_target() {
        Ok(path) => cli::print_kv("config file", &path.display().to_string()),
        Err(e) => cli::print_kv("config file", &e),
    }
    let def = DonsetchConfig::default();
    let modified: Vec<_> = SETTINGS
        .iter()
        .filter(|s| schema::get_value(&loaded.config, s) != schema::get_value(&def, s))
        .collect();
    cli::print_kv(
        "active knobs",
        &format!("{} total · {} non-default", SETTINGS.len(), modified.len()),
    );
    if !modified.is_empty() {
        cli::print_section("Modified Settings");
        for spec in modified {
            let val = schema::get_value(&loaded.config, spec);
            let origin = schema::origin_label(&loaded, spec);
            println!(
                "    {}  {}  {}",
                cli::cyan(&format!("{:<30}", spec.full_key())),
                schema::display_value(spec, &val),
                cli::dim(&format!("({origin})"))
            );
        }
    }
    println!();
    cli::print_nav(&[
        ("donsetch settings list", "all settings"),
        ("donsetch settings set search.sequential true", "edit one"),
        ("donsetch settings preset slow-network", "apply preset"),
    ]);
}

fn cmd_path() {
    match config::config_file_target() {
        Ok(path) => println!("{}", path.display()),
        Err(e) => {
            crate::diag!("{} {e}", cli::icon_fail());
            std::process::exit(1);
        }
    }
}

fn cmd_list(args: &[String]) {
    let loaded = load_or_exit();
    let json_mode = args.iter().any(|a| a == "--json");
    let modified_only = args.iter().any(|a| a == "--modified");
    let mut section_filter: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--section" {
            if let Some(val) = args.get(i + 1) {
                section_filter = Some(val.to_ascii_lowercase());
                i += 1;
            } else {
                crate::diag!("donsetch settings list: --section requires a section name");
                std::process::exit(1);
            }
        } else if let Some(val) = args[i].strip_prefix("--section=") {
            section_filter = Some(val.to_ascii_lowercase());
        }
        i += 1;
    }

    let def = DonsetchConfig::default();
    let items: Vec<_> = SETTINGS
        .iter()
        .filter(|s| {
            section_filter
                .as_ref()
                .is_none_or(|f| s.section == f || s.tab.label().eq_ignore_ascii_case(f))
        })
        .filter(|s| {
            !modified_only || schema::get_value(&loaded.config, s) != schema::get_value(&def, s)
        })
        .collect();

    if json_mode {
        let rows: Vec<_> = items
            .iter()
            .map(|spec| {
                let val = schema::get_value(&loaded.config, spec);
                let def_val = schema::get_value(&def, spec);
                json!({
                    "key": spec.full_key(),
                    "section": spec.section,
                    "tab": spec.tab.label(),
                    "label": spec.label,
                    "value": schema::display_value(spec, &val),
                    "default": schema::display_value(spec, &def_val),
                    "modified": val != def_val,
                    "origin": schema::origin_label(&loaded, spec),
                    "env": spec.env_var(),
                    "summary": spec.summary,
                })
            })
            .collect();
        let doc = json!({
            "config_file": config::config_file_target().ok().map(|p| p.display().to_string()),
            "count": rows.len(),
            "settings": rows,
        });
        println!("{}", serde_json::to_string_pretty(&doc).unwrap_or_default());
        return;
    }

    cli::print_header(
        &format!("{DISPLAY_NAME} Settings"),
        &format!("{} settings", items.len()),
    );
    let mut last_tab: Option<TabId> = None;
    for spec in items {
        if last_tab != Some(spec.tab) {
            cli::print_section(spec.tab.label());
            last_tab = Some(spec.tab);
        }
        let val = schema::get_value(&loaded.config, spec);
        let def_val = schema::get_value(&def, spec);
        let origin = schema::origin_label(&loaded, spec);
        let val_txt = schema::display_value(spec, &val);
        let painted_val = if val != def_val {
            cli::yellow(&val_txt)
        } else {
            val_txt
        };
        println!(
            "    {}  {:<20}  {}",
            cli::cyan(&format!("{:<30}", spec.full_key())),
            painted_val,
            cli::dim(&format!("({origin})  {}", spec.summary))
        );
    }
    println!();
    cli::print_nav(&[
        ("donsetch settings", "open interactive UI"),
        ("donsetch settings set <section.key> <value>", "update"),
    ]);
}

fn cmd_get(args: &[String]) {
    let loaded = load_or_exit();
    let json_mode = args.iter().any(|a| a == "--json");
    let Some(key) = args.iter().find(|a| !a.starts_with('-')) else {
        crate::diag!("Usage: donsetch settings get <section.key> [--json]");
        std::process::exit(1);
    };
    let Some(spec) = schema::find_setting(key) else {
        crate::diag!("{} unknown setting `{key}`", cli::icon_fail());
        let candidates: Vec<String> = SETTINGS.iter().map(SettingSpecExt::full_key).collect();
        let refs: Vec<&str> = candidates.iter().map(String::as_str).collect();
        if let Some(tip) = cli::args::suggest(key, &refs) {
            crate::diag!("  tip: a similar setting exists: `{tip}`");
        }
        std::process::exit(1);
    };
    let val = schema::get_value(&loaded.config, spec);
    let def_val = schema::get_value(&DonsetchConfig::default(), spec);
    let origin = schema::origin_label(&loaded, spec);
    if json_mode {
        let doc = json!({
            "key": spec.full_key(),
            "label": spec.label,
            "value": schema::display_value(spec, &val),
            "default": schema::display_value(spec, &def_val),
            "origin": origin,
            "env": spec.env_var(),
            "summary": spec.summary,
        });
        println!("{}", serde_json::to_string_pretty(&doc).unwrap_or_default());
    } else {
        println!(
            "{} = {} ({origin})",
            spec.full_key(),
            schema::display_value(spec, &val)
        );
    }
}

fn cmd_set(args: &[String]) {
    if args.len() < 2 {
        crate::diag!("Usage: donsetch settings set <section.key> <value>");
        std::process::exit(1);
    }
    let key = args[0].as_str();
    let raw_val = args[1..].join(" ");
    let Some(spec) = schema::find_setting(key) else {
        crate::diag!("{} unknown setting `{key}`", cli::icon_fail());
        let candidates: Vec<String> = SETTINGS.iter().map(SettingSpecExt::full_key).collect();
        let refs: Vec<&str> = candidates.iter().map(String::as_str).collect();
        if let Some(tip) = cli::args::suggest(key, &refs) {
            crate::diag!("  tip: a similar setting exists: `{tip}`");
        }
        std::process::exit(1);
    };
    let parsed = match schema::parse_value(spec, &raw_val) {
        Ok(v) => v,
        Err(e) => {
            crate::diag!("{} {e}", cli::icon_fail());
            std::process::exit(1);
        }
    };
    match config::set_file_values(&[(spec.section, spec.key, parsed.clone())]) {
        Ok(path) => {
            println!(
                "  {} {} = {}",
                cli::icon_pass(),
                cli::bold(&spec.full_key()),
                cli::cyan(&schema::display_value(spec, &parsed))
            );
            cli::print_kv("config", &path.display().to_string());
            cli::print_kv("applies", "next donsetch run / MCP server restart");
        }
        Err(e) => {
            crate::diag!("{} {e}", cli::icon_fail());
            std::process::exit(1);
        }
    }
}

fn cmd_reset(args: &[String]) {
    let Some(key) = args.iter().find(|a| !a.starts_with('-')) else {
        crate::diag!("Usage: donsetch settings reset <section.key>");
        std::process::exit(1);
    };
    let Some(spec) = schema::find_setting(key) else {
        crate::diag!("{} unknown setting `{key}`", cli::icon_fail());
        std::process::exit(1);
    };
    match config::set_file_values(&[(spec.section, spec.key, TomlEditValue::Unset)]) {
        Ok(path) => {
            let def_val = schema::get_value(&DonsetchConfig::default(), spec);
            println!(
                "  {} reset {} to default ({})",
                cli::icon_pass(),
                cli::bold(&spec.full_key()),
                cli::dim(&schema::display_value(spec, &def_val))
            );
            cli::print_kv("config", &path.display().to_string());
        }
        Err(e) => {
            crate::diag!("{} {e}", cli::icon_fail());
            std::process::exit(1);
        }
    }
}

fn cmd_preset(args: &[String]) {
    let json_mode = args.iter().any(|a| a == "--json");
    let chosen = args
        .iter()
        .find(|a| !a.starts_with('-'))
        .map(String::as_str);
    let Some(name) = chosen else {
        if json_mode {
            let list: Vec<_> = PRESETS
                .iter()
                .map(|p| {
                    json!({
                        "id": p.id,
                        "name": p.name,
                        "tagline": p.tagline,
                        "changes": p.changes.iter().map(|(s, k, v)| json!({"key": format!("{s}.{k}"), "value": v})).collect::<Vec<_>>(),
                    })
                })
                .collect();
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({ "presets": list })).unwrap_or_default()
            );
            return;
        }
        cli::print_header(
            &format!("{DISPLAY_NAME} Settings Presets"),
            "curated profiles",
        );
        println!();
        for p in PRESETS {
            println!(
                "  {}  {}",
                cli::cyan(&format!("{:<22}", p.id)),
                cli::bold(p.name)
            );
            println!("    {}", cli::dim(p.tagline));
        }
        println!();
        cli::print_nav(&[("donsetch settings preset slow-network", "apply preset")]);
        return;
    };

    let Some(preset) = PRESETS.iter().find(|p| p.id.eq_ignore_ascii_case(name)) else {
        crate::diag!("{} unknown preset `{name}`", cli::icon_fail());
        let ids: Vec<&str> = PRESETS.iter().map(|p| p.id).collect();
        if let Some(tip) = cli::args::suggest(name, &ids) {
            crate::diag!("  tip: a similar preset exists: `{tip}`");
        }
        crate::diag!("  available: {}", ids.join(", "));
        std::process::exit(1);
    };

    let mut updates = Vec::new();
    for (sec, key, raw) in preset.changes {
        let spec = schema::find_setting(&format!("{sec}.{key}")).expect("preset setting exists");
        let parsed = match schema::parse_value(spec, raw) {
            Ok(v) => v,
            Err(e) => {
                crate::diag!("{} {e}", cli::icon_fail());
                std::process::exit(1);
            }
        };
        updates.push((*sec, *key, parsed));
    }
    match config::set_file_values(&updates) {
        Ok(path) => {
            if json_mode {
                let doc = json!({
                    "ok": true,
                    "preset": preset.id,
                    "config_file": path.display().to_string(),
                    "applied": updates.len(),
                });
                println!("{}", serde_json::to_string_pretty(&doc).unwrap_or_default());
                return;
            }
            cli::print_header(&format!("{DISPLAY_NAME} Settings"), preset.name);
            println!();
            for (sec, key, val) in &updates {
                let spec = schema::find_setting(&format!("{sec}.{key}")).unwrap();
                cli::check_pass(&spec.full_key(), &schema::display_value(spec, val));
            }
            println!();
            cli::print_kv("config", &path.display().to_string());
            cli::print_kv("applies", "next donsetch run / MCP server restart");
        }
        Err(e) => {
            crate::diag!("{} {e}", cli::icon_fail());
            std::process::exit(1);
        }
    }
}

trait SettingSpecExt {
    fn full_key(&self) -> String;
}

impl SettingSpecExt for schema::SettingSpec {
    fn full_key(&self) -> String {
        schema::SettingSpec::full_key(self)
    }
}

pub fn help() {
    cli::print_header(
        &format!("{DISPLAY_NAME} Settings"),
        "interactive Control Center & config editor",
    );
    println!();
    println!(
        "  {} {}",
        cli::dim("Usage:"),
        cli::bold("donsetch settings [subcommand]")
    );

    cli::print_section("Interactive Control Center");
    cli::print_opt(
        "(no arguments)",
        "Open the interactive TUI (toggles, sliders, BYOK keys, proxies, rules, presets)",
    );

    cli::print_section("Scriptable Subcommands (Agents & Automation)");
    cli::print_opt(
        "list [--section S]",
        "List active settings (--modified, --json)",
    );
    cli::print_opt(
        "get <section.key>",
        "Show one setting's value and source (--json)",
    );
    cli::print_opt(
        "set <key> <val>",
        "Validate and persist one setting to donsetch.toml",
    );
    cli::print_opt(
        "reset <key>",
        "Remove an override from donsetch.toml (use default)",
    );
    cli::print_opt(
        "preset [name]",
        "List or apply a preset (slow-network, balanced, low-memory)",
    );
    cli::print_opt("path", "Print the active donsetch.toml target path");

    println!();
    println!(
        "  {}",
        cli::dim("Alias: `donsetch setting` works identically to `donsetch settings`.")
    );
    println!();
    cli::print_nav(&[
        ("donsetch settings", "launch TUI"),
        ("donsetch settings preset slow-network", "sequential search"),
        ("donsetch config show", "layered origin table"),
    ]);
}
