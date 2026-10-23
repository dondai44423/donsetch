//! doncore — the one local fetching system (v5).
//!
//! One engine, one code path: the wire (BoringSSL via wreq), the cookie jar,
//! the HTTP cache, the learned per-host state and the QuickJS runtime share
//! one identity record, one deadline and one set of connections. A page that
//! demands JS gets its own scripts executed against a browser-faithful
//! environment whose network calls go back through the same wire.
//!
//! The `doncore` binary is the same library with a thin argument surface — the
//! private lab harness (`v5/doncore/tools/`) drives it. The product (donsetch)
//! links this library.

pub mod engine;
pub mod identity;
pub mod rt;
pub mod state;
pub mod wire;
