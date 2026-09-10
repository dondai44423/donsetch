pub mod h1;
pub mod h2;
pub mod h3;
pub mod pool;
pub mod proxy;
pub mod routes;
pub mod tcp;
pub mod tls;

/// Hard cap on a response body, shared by every transport (matches
/// the decompression cap : bombs must fail before they allocate).
pub(crate) const MAX_BODY: usize = 64 << 20;
