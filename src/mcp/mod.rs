//! DonSeTch MCP daemon — JSON-RPC 2.0 over stdio (NDJSON) and HTTP/SSE.
//!
//! Stdio mode: one message per line. Requests spawn tasks; responses
//! funnel through a single writer task so lines never interleave.
//!
//! HTTP mode: SSE streaming for responses and progress notifications.

pub mod http;
pub mod server;
pub mod stdio;
pub mod supervisor;
pub mod tools;
