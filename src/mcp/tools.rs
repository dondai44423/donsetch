//! Tool schemas for the MCP tools/list response.
//!
//! Descriptions are LLM-optimized: dense, self-contained,
//! and actionable. An agent reading only the description
//! (never our source) should know exactly when to call,
//! which params to set, and how to interpret the response.
//!
//! Schemas are GENERATED from `crate::spec::TOOLS` : the
//! single source of truth shared with the CLI. Never edit
//! schemas here; edit the spec table.

use serde_json::{Value, json};

/// Protocol versions we speak, newest first.
pub const PROTOCOL_VERSIONS: &[&str] = &[
    "2026-07-28",
    "2025-11-25",
    "2025-06-18",
    "2025-03-26",
    "2024-11-05",
    "2024-10-07",
];

pub const SERVER_NAME: &str = "donsetch";

/// Human-readable label for UI surfaces: MCP keeps `name` programmatic and
/// `title` for display (optional since 2025-06-18, ignored by older clients).
/// Shared with the exe's FileDescription : see src/display_name.rs.
pub const SERVER_TITLE: &str = crate::DISPLAY_NAME;
pub const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");

/// The `instructions` string sent at initialize. Generated from
/// the spec table : a new tool appears here with no prose edit.
pub fn instructions() -> String {
    // Name + summary, one per line. Summaries are already
    // one-liners : they double as the `--help` subcommand listing.
    let tools = crate::spec::TOOLS
        .iter()
        .map(|t| format!("- {0}: {1}", t.name, t.summary))
        .collect::<Vec<_>>()
        .join("\n");
    // Clients inject this alongside the system prompt, and unlike
    // tool descriptions it arrives unconditionally: harnesses with
    // deferred tool loading (Claude Code's ToolSearch) show only
    // tool NAMES up front and fetch schemas on demand, so this is
    // what tells an agent we exist. It is resident in every
    // session whether or not we are used : keep it short.
    // tests/token_invariants.rs gates the size.
    format!(
        "Web access: fetch, search, crawl : one URL, many URLs, or a whole site.\
        \n\n{tools}\n\n\
        Output is the page's own markdown : full wording, code blocks and tables preserved."
    )
}

/// tools/list payload : generated from the spec table.
pub fn list() -> Value {
    json!({
        "tools": crate::spec::TOOLS.iter().map(crate::spec::mcp_schema).collect::<Vec<_>>()
    })
}

#[cfg(test)]
mod tests {
    /// Golden fixture: the generated tools/list must be
    /// byte-identical (as a Value) to the pre-refactor
    /// hand-written schema. If this fails, the spec table
    /// drifted from the shipped MCP contract.
    #[test]
    fn generated_schema_matches_fixture() {
        let fixture: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/fixtures/tools_list.json"
            ))
            .expect("read fixture"),
        )
        .expect("parse fixture");
        assert_eq!(super::list(), fixture, "tools/list drifted from fixture");
    }
}
