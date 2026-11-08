//! DonSeTch : web fetch, search and crawl for AI agents.
//!
//! Library surface (used by the `donsetch` binary, the fuzz
//! targets, and integration tests):
//!
//! - [`extract`] : DonSift: HTML/PDF bytes → agent-native markdown
//! - [`fetch`] : the fetch pipeline riding the in-tree engine
//! - [`browser`] : the locally installed browser (screenshot, parity)
//! - [`state`] : the disk-backed fetch state (route memory, wall memory, personas)
//! - [`search`] : DonSeek: keyless multi-engine search
//! - [`crawl`] : DonTread: sitemap-aware site walking
//! - [`detect`] : wall/bot detection verdicts
//! - [`rules`] : the operator's per-domain rules (deny)
//! - [`mcp`] : the stdio MCP daemon + tool dispatch
//! - [`spec`] : the tool spec table (MCP + CLI are generated from it)
//!
//! The binary in `main.rs` is a thin shell over this library.

pub mod adapters;
pub mod browser;
pub mod cli;
pub mod config;
pub mod cpu;
pub mod crawl;
pub mod detect;
pub mod dev;
pub mod diag;
// Also read verbatim by build.rs : see the file.
mod display_name;
pub use display_name::DISPLAY_NAME;
pub mod egress;
pub mod error;
pub mod extract;
pub mod fetch;
pub mod handles;
pub mod mcp;
pub mod onnx;
pub mod pages;
pub mod paths;
pub mod pdf;
pub mod persona;
pub mod profile;
pub mod rules;
pub mod search;
pub mod spec;
pub mod state;
pub mod state_probe;
