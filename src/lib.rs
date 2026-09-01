//! DonSeTch : web fetch, search and crawl for AI agents.
//!
//! Library surface (used by the `donsetch` binary, the fuzz
//! targets, and integration tests):
//!
//! - [`extract`] : DonSift: HTML/PDF bytes → agent-native markdown
//! - [`fetch`] : DonShadow: the tier-1 stealthy HTTP client
//! - [`ghost`] : DonGhost: tier-2 headless-Chromium escalation
//! - [`search`] : DonSeek: keyless multi-engine search
//! - [`crawl`] : DonTread: sitemap-aware site walking
//! - [`detect`] : wall/bot detection verdicts
//! - [`mcp`] : the stdio MCP daemon + tool dispatch
//! - [`spec`] : the tool spec table (MCP + CLI are generated from it)
//!
//! The binary in `main.rs` is a thin shell over this library.

pub mod adapters;
pub mod auth;
pub mod cli;
mod config;
pub mod cpu;
pub mod crawl;
pub mod detect;
pub mod dev;
pub mod error;
pub mod extract;
pub mod fetch;
pub mod ghost;
pub mod handles;
pub mod mcp;
pub mod memory;
pub mod onnx;
pub mod pages;
pub mod paths;
pub mod pdf;
pub mod profile;
pub mod search;
pub mod spec;
pub mod transport;
