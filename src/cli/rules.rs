//! `donsetch rules`: the human face of the local `[rules]` table.
//!
//! `rules test <url>` explains, offline, what the operator's rules do
//! with one URL: every matching key in precedence order, the winner,
//! and the winner's fields with the config layer each came from. It
//! never touches the network. The `<command> --help` text lives at the
//! bottom.

use std::fmt::Write as _;

use crate::cli::tool::{EXIT_PERMANENT, EXIT_WALLED};
use crate::config;
use crate::rules::{CompiledRule, DenyKind, RuleAction, RuleSet, RulesMode, RulesSection};

/// Run `donsetch rules <subcommand>`. `args` is the full argv
/// (`args[1]` is `rules`). Exits the process with 1 on a usage, config
/// or URL error.
pub fn run(args: &[String]) {
    match args.get(2).map(String::as_str) {
        Some("test") => {
            crate::cli::args::Spec::new("rules test", "donsetch rules test <url>", &[])
                .reject(&args[3..]);
        }
        Some("help" | "--help" | "-h") => {
            help();
            return;
        }
        None => {
            help();
            std::process::exit(1);
        }
        Some(other) => {
            crate::diag!("donsetch rules: unknown subcommand '{other}'\n");
            help();
            std::process::exit(1);
        }
    }
    let rest = &args[3..];
    if rest.iter().any(|a| a == "--help" || a == "-h") {
        help();
        return;
    }
    let [url_arg] = rest else {
        crate::diag!("Usage: donsetch rules test <url>");
        std::process::exit(1);
    };

    // main.rs has already installed this config (a load error stopped
    // it there); loading again is how the CLI gets the merged tree with
    // its per-leaf origins, which the installed config does not expose.
    let loaded = match config::load() {
        Ok(loaded) => loaded,
        Err(e) => {
            crate::diag!("donsetch: {e}");
            std::process::exit(1);
        }
    };
    let set = match RuleSet::compile(&loaded.config.rules) {
        Ok(set) => set,
        Err(e) => {
            crate::diag!("donsetch: {e}");
            std::process::exit(1);
        }
    };
    let url = match url::Url::parse(url_arg) {
        Ok(url) => url,
        Err(e) => {
            crate::diag!(
                "donsetch rules test: {url_arg:?} is not an absolute URL ({e}); \
                 write it with its scheme, e.g. https://example.com/"
            );
            std::process::exit(1);
        }
    };
    print!(
        "{}",
        render_test(&url, &loaded.config.rules, &set, &|path| {
            loaded.origin_of_path(path).map(str::to_string)
        })
    );
}

/// Maps a config path (`["rules", "mode"]`, `["rules", "url", key,
/// field]`) to the layer that set it; `None` means the default.
type Origin<'a> = dyn Fn(&[&str]) -> Option<String> + 'a;

/// The layer label for `path`, `"default"` when no layer set it.
fn layer(origin: &Origin<'_>, path: &[&str]) -> String {
    origin(path).unwrap_or_else(|| "default".to_string())
}

/// The full `rules test` report for `url`. `set` must be compiled from
/// `section`.
fn render_test(
    url: &url::Url,
    section: &RulesSection,
    set: &RuleSet,
    origin: &Origin<'_>,
) -> String {
    let mut out = String::new();

    // The off state goes first, before any match, so a forgotten
    // DONSETCH_RULES__MODE=off cannot hide below a list of rules that
    // look live.
    if section.mode == RulesMode::Off {
        let _ = writeln!(
            out,
            "Rules are OFF (mode = \"{}\", from {}): nothing below is enforced.",
            section.mode.as_str(),
            layer(origin, &["rules", "mode"])
        );
        out.push('\n');
    }

    let _ = writeln!(out, "URL   {url}");
    let _ = writeln!(out, "host  {}", url.host_str().unwrap_or("(none)"));
    out.push('\n');

    let matches = set.matches(url);
    let winner = set.eval(url);
    if matches.is_empty() {
        let _ = writeln!(
            out,
            "No rule matches this URL: it is fetched as if no rule existed."
        );
        return out;
    }

    let width = matches.iter().map(|r| r.key.len()).max().unwrap_or(0);
    let _ = writeln!(out, "Matching rules, most specific first:");
    for rule in &matches {
        let wins = winner.is_some_and(|w| w.key == rule.key);
        let (mark, status) = if wins {
            ("*", "wins")
        } else if !rule.rule.enabled {
            (" ", "disabled (enabled = false)")
        } else {
            (" ", "beaten by a more specific rule")
        };
        let _ = writeln!(out, "  {mark} {:<width$}  {status}", rule.key);
    }
    out.push('\n');

    let Some(winner) = winner else {
        let _ = writeln!(
            out,
            "No enabled rule matches: the URL is fetched as if no rule existed."
        );
        return out;
    };
    render_winner(&mut out, winner, origin);
    out
}

/// The winning rule's fields, each with its layer, and what the rule
/// does to a fetch of `url`.
fn render_winner(out: &mut String, winner: &CompiledRule, origin: &Origin<'_>) {
    let key = winner.key.as_str();
    let rule = &winner.rule;
    let _ = writeln!(out, "Winning rule: [rules.url.\"{key}\"]");
    let field = |out: &mut String, name: &str, value: &str| {
        let _ = writeln!(
            out,
            "  {name:<13} {value:<24} ({})",
            layer(origin, &["rules", "url", key, name])
        );
    };
    // Only the fields that apply to the merged action are shown: a
    // carve-out that turned an inherited deny into an allow still
    // carries the deny's message and kind, which do nothing.
    match rule.action {
        RuleAction::Deny => {
            field(out, "action", "deny");
            field(out, "kind", rule.kind.as_str());
            field(out, "reason", rule.reason.as_deref().unwrap_or("(none)"));
            field(out, "message", rule.message.as_deref().unwrap_or(""));
            let code = winner
                .denial()
                .map(|d| d.code())
                .unwrap_or_else(|| crate::rules::policy_code(rule.reason.as_deref()));
            let exit = match rule.kind {
                DenyKind::Walled => EXIT_WALLED,
                DenyKind::Permanent => EXIT_PERMANENT,
            };
            let _ = writeln!(
                out,
                "Effect: denied before any request: code {code}, errorKind {} ({}), CLI exit {exit}.",
                rule.kind.as_str(),
                rule.kind.phrase()
            );
        }
        RuleAction::Allow => {
            field(out, "action", "allow");
            let _ = writeln!(out, "Effect: allowed.");
        }
    }
}

/// Usage text for `donsetch rules --help` and `help rules`.
pub fn help() {
    println!("Usage: donsetch rules test <url>");
    println!();
    println!("  Explain what the local DonSeTch rules ([rules] in donsetch.toml) do");
    println!("  with one URL, offline: every matching rule, most specific first, the");
    println!("  one that wins, and the winner's fields with the config layer each came");
    println!("  from. Nothing is fetched.");
    println!();
    println!("  The most specific rule wins in full (a narrower rule replaces a broader");
    println!("  one, action included). Rules are switched off by [rules] mode = \"off\",");
    println!("  DONSETCH_RULES__MODE=off, or `--ignore-rules` on fetch, crawl and");
    println!("  screenshot.");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rules::UrlRule;

    fn section(rules: &[(&str, UrlRule)]) -> RulesSection {
        let mut s = RulesSection::default();
        for (key, rule) in rules {
            s.url.insert((*key).to_string(), rule.clone());
        }
        s
    }

    fn deny(message: &str) -> UrlRule {
        UrlRule {
            action: RuleAction::Deny,
            message: Some(message.to_string()),
            reason: Some("ip_ban".to_string()),
            ..UrlRule::default()
        }
    }

    fn render(url: &str, section: &RulesSection) -> String {
        render_with(url, section, &|_| None)
    }

    fn render_with(url: &str, section: &RulesSection, origin: &Origin<'_>) -> String {
        let set = RuleSet::compile(section).expect("test rules compile");
        render_test(&url::Url::parse(url).unwrap(), section, &set, origin)
    }

    #[test]
    fn mode_off_is_stated_before_any_match() {
        let mut s = section(&[("example.com", deny("go elsewhere"))]);
        s.mode = RulesMode::Off;
        let out = render_with("https://example.com/", &s, &|path| {
            (path == ["rules", "mode"]).then(|| "DONSETCH_RULES__MODE".to_string())
        });
        assert!(
            out.starts_with(
                "Rules are OFF (mode = \"off\", from DONSETCH_RULES__MODE): nothing below is enforced."
            ),
            "{out}"
        );
        // The matches are still explained, below the banner.
        assert!(out.contains("* example.com"), "{out}");
    }

    #[test]
    fn mode_enforce_prints_no_off_banner() {
        let s = section(&[("example.com", deny("go elsewhere"))]);
        let out = render("https://example.com/", &s);
        assert!(!out.contains("OFF"), "{out}");
        assert!(out.starts_with("URL "), "{out}");
    }

    #[test]
    fn every_match_is_listed_in_precedence_order_and_the_winner_is_marked() {
        let disabled = UrlRule {
            enabled: false,
            ..deny("disabled deny")
        };
        let s = section(&[
            ("example.com", deny("the whole tree")),
            ("www.example.com", UrlRule::default()),
            ("https://www.example.com", disabled),
            ("other.org", deny("never matches")),
        ]);
        let out = render("https://www.example.com/page", &s);
        let list: Vec<&str> = out
            .lines()
            .skip_while(|l| !l.starts_with("Matching rules"))
            .skip(1)
            .take_while(|l| !l.is_empty())
            .collect();
        assert_eq!(list.len(), 3, "{out}");
        assert!(
            list[0].trim_start().starts_with("https://www.example.com")
                && list[0].contains("disabled"),
            "{out}"
        );
        assert!(
            list[1].trim_start().starts_with("* www.example.com"),
            "{out}"
        );
        assert!(list[1].ends_with("wins"), "{out}");
        assert!(list[2].trim_start().starts_with("example.com"), "{out}");
        assert!(list[2].contains("beaten"), "{out}");
        assert!(!out.contains("other.org"), "{out}");
        assert!(
            out.contains("Winning rule: [rules.url.\"www.example.com\"]"),
            "{out}"
        );
    }

    #[test]
    fn winner_fields_name_the_layer_each_came_from() {
        let s = section(&[("www.example.com", UrlRule::default())]);
        let out = render_with("https://www.example.com/", &s, &|path| {
            (path == ["rules", "url", "www.example.com", "action"])
                .then(|| "file:/etc/donsetch.toml".to_string())
        });
        let line = |name: &str| {
            out.lines()
                .find(|l| l.trim_start().starts_with(name))
                .unwrap_or_else(|| panic!("no {name} line: {out}"))
                .to_string()
        };
        assert!(
            line("action").ends_with("(file:/etc/donsetch.toml)"),
            "{out}"
        );
    }

    #[test]
    fn a_deny_winner_shows_its_code_kind_and_exit_code() {
        let s = section(&[("banned.example", deny("use BladeBrowser"))]);
        let out = render("https://www.banned.example/publication/1", &s);
        assert!(out.contains("code policy.denied.ip_ban"), "{out}");
        assert!(out.contains("errorKind walled"), "{out}");
        assert!(out.contains(&format!("CLI exit {EXIT_WALLED}")), "{out}");
        assert!(out.contains("use BladeBrowser"), "{out}");
    }

    #[test]
    fn a_url_no_rule_matches_says_so() {
        let s = section(&[("example.com", deny("go elsewhere"))]);
        let out = render("https://example.org/", &s);
        assert!(out.contains("No rule matches this URL"), "{out}");
        assert!(!out.contains("Winning rule"), "{out}");
    }

    #[test]
    fn only_disabled_matches_leave_no_winner() {
        let disabled = UrlRule {
            enabled: false,
            ..deny("off")
        };
        let s = section(&[("example.com", disabled)]);
        let out = render("https://example.com/", &s);
        assert!(out.contains("disabled (enabled = false)"), "{out}");
        assert!(out.contains("No enabled rule matches"), "{out}");
        assert!(!out.contains("Winning rule"), "{out}");
    }
}
