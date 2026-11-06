//! The model that outlived the transport (v5 swap, 2026-10-23).
//!
//! The wire itself is the fetch core (`v5core`): these modules are the app's
//! pure model around it. `proxy` parses and persists egress lanes,
//! `request_route` is the immutable routing policy shared by a fetch and
//! its related reads, `dns` is the process-local resolver cache the
//! SSRF guard resolves through, and `trust` keeps the doctor's trust-store
//! inventory.

pub mod dns;
pub mod proxy;
pub mod request_route;
pub mod trust;

/// Hard cap on a response body for the app-side readers that still read
/// without the engine (the unlocker's reqwest path, the BYOK SERP reader).
/// Bombs must fail before they allocate.
pub(crate) const MAX_BODY: usize = 64 << 20;
