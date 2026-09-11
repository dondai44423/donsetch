//! MCP client-compat layer for harnesses that show the model only
//! one surface and discard the other.
//!
//! Two observed failure modes:
//! - `structuredContent` only, `content` dropped (Claude Code, VS Code:
//!   the model sees tool metadata but never the document).
//! - `content` only, `structuredContent` dropped (OpenCode v1: the model
//!   sees the document but never the compact state).
//!
//! DonSeTch's shape is deliberately split: `content` carries the
//! document markdown, `structuredContent` carries compact actionable
//! state (verdict, next_offset, resume tokens, error codes). Clients
//! that render both lose nothing; clients that pick one need the
//! surfaces merged into the one they actually show.
//!
//! Three layers, explicit precedence:
//! 1. Default: unchanged split shape (token-optimal) for every
//!    client not recognized and with no override set.
//! 2. Handshake detection: `clientInfo.name` on `initialize` matching
//!    a known text-only client flips that session to TextOnly.
//! 3. Manual override: `DONSETCH_MCP_TEXT_ONLY=1` forces TextOnly for
//!    every client regardless of handshake, so a newly-discovered
//!    broken host can be fixed today without waiting for a release.
//!
//! TextOnly shape (validated end to end by the issue reporter's own
//! stdio proxy): the full `structuredContent` is folded into a compact
//! leading `[meta]` text block (lossless by construction, ~10% of the
//! document), `structuredContent` is dropped, and the document stays a
//! clean markdown text block. Every tool folds the same way,
//! web_search included: its model-facing `structuredContent` is the
//! lean `{weak, results:[{rank,url,handle}]}` routing state built by
//! `search_model_meta`, while the titles and snippets live in the
//! markdown block. The per-result title/snippet/score/engines view
//! is `search_debug_meta`, which rides in `_meta` and never reaches
//! the model in either mode.

use std::sync::atomic::{AtomicU8, Ordering};

use serde_json::{Value, json};

/// Per-session mode holder. Shared by every request of one session:
/// `initialize` writes it, tool calls read it.
pub struct ModeCell(AtomicU8);

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum ClientMode {
    /// Split shape: document in `content`, state in `structuredContent`.
    #[default]
    Default,
    /// One surface: state folded into a leading `[meta]` text block,
    /// `structuredContent` dropped.
    TextOnly,
}

impl ModeCell {
    pub fn new() -> Self {
        Self(AtomicU8::new(0))
    }

    pub fn set(&self, mode: ClientMode) {
        self.0.store(mode as u8, Ordering::Relaxed);
    }

    pub fn get(&self) -> ClientMode {
        if self.0.load(Ordering::Relaxed) == ClientMode::TextOnly as u8 {
            ClientMode::TextOnly
        } else {
            ClientMode::Default
        }
    }
}

impl Default for ModeCell {
    fn default() -> Self {
        Self::new()
    }
}

/// Clients observed showing the model only one surface and discarding
/// the other. Matched case-insensitively, exact.
///
/// - Claude Code / VS Code: keep `structuredContent`, drop `content`.
/// - OpenCode v1: keep `content`, drop `structuredContent`.
///
/// Wrappers reusing one of these names get one redundant surface at
/// worst (a client that renders text AND matches here sees the meta
/// block next to nothing else), so defaulting to a name match is the
/// safe direction.
///
/// EXIT PLAN: when a listed client starts rendering both surfaces,
/// delete the entry and re-run a single `web_fetch` through it;
/// if the document reaches the agent, the entry stays deleted.
const TEXT_ONLY_CLIENTS: &[&str] = &[
    "claude-code", // renders structuredContent
    "opencode",    // renders only content
    "vscode",      // renders structuredContent
];

/// Handshake-detected mode from `clientInfo.name`. Lenient by
/// construction: the caller hands over the raw JSON-RPC params, and
/// `clientInfo` may be missing, nameless, or shaped arbitrarily
/// (Claude Code sends `version` as an OBJECT, so this layer reads the
/// name only and never deserializes a strict `Implementation`).
pub fn mode_from_params(params: &Value) -> ClientMode {
    let name = params
        .pointer("/clientInfo/name")
        .and_then(Value::as_str)
        .unwrap_or("");
    let name = name.trim().to_ascii_lowercase();
    if TEXT_ONLY_CLIENTS.contains(&name.as_str()) {
        ClientMode::TextOnly
    } else {
        ClientMode::Default
    }
}

/// Manual override. Fail-closed parse (same convention as the other
/// env flags): only an explicit true value turns it on; unset, empty,
/// or any other value leaves the handshake in charge.
pub fn env_override() -> Option<ClientMode> {
    if crate::config::env_flag("DONSETCH_MCP_TEXT_ONLY") {
        Some(ClientMode::TextOnly)
    } else {
        None
    }
}

/// The mode a session's tool responses take: explicit override first,
/// then whatever the handshake detected, then the default split shape.
pub fn effective(cell: &ModeCell) -> ClientMode {
    env_override().unwrap_or_else(|| cell.get())
}

/// Merge the split surfaces for a text-only client. Applies to every
/// tool: whatever `structuredContent` a result carries is state the
/// model needs, and a client in this mode drops the `content` array
/// that holds the document.
pub fn shape_result(mut result: Value) -> Value {
    let Some(sc) = result.get("structuredContent") else {
        return result;
    };
    let Value::Object(map) = sc else {
        return result;
    };
    let mut meta = map.clone();
    // crawl mode=map renders the URL inventory into the text body;
    // folding it into the meta block would double a large crawl.
    meta.remove("map");
    if meta.is_empty() {
        return result;
    }
    // Prepend, not append: next_offset / resume / next_action stay
    // visible even when only the head of a long response is read.
    // A separate block, never concatenated: agents read the two
    // surfaces as different kinds of information.
    let meta_text = format!("[meta] {}", Value::Object(meta));
    let mut content = result
        .get("content")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    content.insert(0, json!({ "type": "text", "text": meta_text }));
    result["content"] = Value::Array(content);
    result
        .as_object_mut()
        .expect("result is an object")
        .remove("structuredContent");
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fetch_result() -> Value {
        json!({
            "content": [{ "type": "text", "text": "# Example page\n\nbody" }],
            "structuredContent": {
                "url": "https://example.com/",
                "content_ok": true,
                "thin": false,
                "lang": "en",
                "next_offset": 4096,
                "total_chars": 100_000,
            }
        })
    }

    #[test]
    fn fetch_result_folds_state_into_leading_meta_block() {
        let shaped = shape_result(fetch_result());
        assert!(shaped.get("structuredContent").is_none());
        let content = shaped["content"].as_array().unwrap();
        assert_eq!(content.len(), 2);
        let meta = content[0]["text"].as_str().unwrap();
        assert!(meta.starts_with("[meta] "));
        assert!(meta.contains("\"next_offset\":4096"));
        assert!(meta.contains("content_ok"));
        // The document block is untouched and second.
        assert_eq!(content[1]["text"], "# Example page\n\nbody");
        assert_eq!(content[1]["type"], "text");
    }

    #[test]
    fn crawl_map_is_not_doubled_into_meta() {
        let crawl = json!({
            "content": [{ "type": "text", "text": "/a\n/b\n" }],
            "structuredContent": { "map": ["/a", "/b"], "stop": "FrontierEmpty" }
        });
        let shaped = shape_result(crawl);
        assert!(shaped.get("structuredContent").is_none());
        let meta = shaped["content"][0]["text"].as_str().unwrap();
        assert!(!meta.contains("\"map\""));
        assert!(meta.contains("FrontierEmpty"));
        // The resume token must survive curation (a curated field
        // list once destroyed it).
        let with_resume = json!({
            "content": [{ "type": "text", "text": "page" }],
            "structuredContent": { "resume": "tok-1", "stop": "MaxPages", "map": [] }
        });
        let shaped = shape_result(with_resume);
        assert!(
            shaped["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("tok-1")
        );
    }

    #[test]
    fn error_results_carry_code_and_next_action_into_meta() {
        let err = json!({
            "content": [{ "type": "text", "text": "fetch failed\n\nNext action: retry once" }],
            "isError": true,
            "errorKind": "transient",
            "code": "network.timeout",
            "structuredContent": { "code": "network.timeout", "escalation": [1, 2] }
        });
        let shaped = shape_result(err);
        assert!(shaped.get("structuredContent").is_none());
        let meta = shaped["content"][0]["text"].as_str().unwrap();
        assert!(meta.contains("network.timeout"));
        assert!(meta.contains("escalation"));
        // isError + the friendly text ride along.
        assert_eq!(shaped["isError"], true);
        assert_eq!(
            shaped["content"][1]["text"],
            "fetch failed\n\nNext action: retry once"
        );
    }

    #[test]
    fn results_without_structured_content_pass_through() {
        let bare = json!({ "content": [{ "type": "text", "text": "x" }] });
        assert_eq!(shape_result(bare.clone()), bare);
        // structuredContent present but empty after the map drop:
        // nothing to fold, shape unchanged.
        let empty = json!({
            "content": [{ "type": "text", "text": "x" }],
            "structuredContent": { "map": ["/a"] }
        });
        assert_eq!(shape_result(empty.clone()), empty);
    }

    #[test]
    fn result_without_content_array_gets_meta_only_content() {
        let odd = json!({ "structuredContent": { "stop": "Deadline" } });
        let shaped = shape_result(odd);
        assert!(shaped.get("structuredContent").is_none());
        assert_eq!(shaped["content"][0]["type"], "text");
        assert!(
            shaped["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("Deadline")
        );
    }

    #[test]
    fn client_name_detection_is_exact_and_case_insensitive() {
        let cc = json!({ "clientInfo": { "name": "claude-code", "version": "1.0" } });
        assert_eq!(mode_from_params(&cc), ClientMode::TextOnly);
        let cc_upper = json!({ "clientInfo": { "name": "Claude-Code" } });
        assert_eq!(mode_from_params(&cc_upper), ClientMode::TextOnly);
        // A wrapper whose name merely CONTAINS the marker is NOT a
        // match: unknown clients keep the token-optimal default.
        let wrapper = json!({ "clientInfo": { "name": "claude-code-proxy" } });
        assert_eq!(mode_from_params(&wrapper), ClientMode::Default);
        // Listed for the OPPOSITE reason (renders content, drops
        // structuredContent), and folded all the same.
        let oc = json!({ "clientInfo": { "name": "opencode" } });
        assert_eq!(mode_from_params(&oc), ClientMode::TextOnly);
        let vs = json!({ "clientInfo": { "name": "vscode" } });
        assert_eq!(mode_from_params(&vs), ClientMode::TextOnly);
        // An unlisted client keeps the token-optimal split shape.
        let other = json!({ "clientInfo": { "name": "cursor" } });
        assert_eq!(mode_from_params(&other), ClientMode::Default);
    }

    #[test]
    fn malformed_client_info_is_lenient() {
        // Claude Code's real handshake shape: version is an OBJECT.
        let claude = json!({
            "clientInfo": {
                "name": "claude-code",
                "version": { "VERSION": "2.1.42", "ISSUES_EXPLAINER": "x" }
            }
        });
        assert_eq!(mode_from_params(&claude), ClientMode::TextOnly);
        // Missing clientInfo entirely.
        assert_eq!(mode_from_params(&json!({})), ClientMode::Default);
        // Non-string name.
        let weird = json!({ "clientInfo": { "name": 42 } });
        assert_eq!(mode_from_params(&weird), ClientMode::Default);
        // clientInfo as a non-object.
        assert_eq!(
            mode_from_params(&json!({ "clientInfo": "x" })),
            ClientMode::Default
        );
    }

    #[test]
    fn mode_cell_roundtrips() {
        let cell = ModeCell::new();
        assert_eq!(cell.get(), ClientMode::Default);
        cell.set(ClientMode::TextOnly);
        assert_eq!(cell.get(), ClientMode::TextOnly);
        cell.set(ClientMode::Default);
        assert_eq!(cell.get(), ClientMode::Default);
    }

    // Env-var test: nextest isolates each test in its own process.
    #[test]
    fn env_override_forces_text_only_for_any_client() {
        unsafe {
            std::env::remove_var("DONSETCH_MCP_TEXT_ONLY");
        }
        assert_eq!(env_override(), None);
        unsafe {
            std::env::set_var("DONSETCH_MCP_TEXT_ONLY", "1");
        }
        assert_eq!(env_override(), Some(ClientMode::TextOnly));
        let cell = ModeCell::new();
        assert_eq!(effective(&cell), ClientMode::TextOnly);
        // =false must NOT enable it (fail-closed flag parse).
        unsafe {
            std::env::set_var("DONSETCH_MCP_TEXT_ONLY", "false");
        }
        assert_eq!(env_override(), None);
        unsafe {
            std::env::remove_var("DONSETCH_MCP_TEXT_ONLY");
        }
    }
}
