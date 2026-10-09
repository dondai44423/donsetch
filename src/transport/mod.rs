pub mod dns;
pub mod h1;
pub mod h2;
pub mod h3;
pub mod pool;
pub mod proxy;
pub mod request_route;
pub mod routes;
pub mod tcp;
pub mod tls;

/// Hard cap on a response body, shared by every transport (matches
/// the decompression cap : bombs must fail before they allocate).
pub(crate) const MAX_BODY: usize = 64 << 20;

/// No-progress bound on one read: a peer must deliver bytes within
/// this window of the previous delivery, while a transfer that keeps
/// moving may take as long as the link needs. A wall-clock bound
/// around a whole response instead failed every healthy transfer
/// slower than the cap: `donsetch -u` died with "Download failed:
/// timeout" on links under ~5Mbps while a 19MB asset's bytes were
/// still arriving steadily (measured: 30s of continuous ~100KB/s
/// flow, killed at exactly +30s, twice).
pub(crate) const STALL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Read under the stall bound: EOF and bytes pass through, and a read
/// that stays empty for STALL_TIMEOUT is `FetchError::Timeout`.
pub(crate) async fn read_stall<S>(
    stream: &mut S,
    buf: &mut [u8],
) -> Result<usize, crate::error::FetchError>
where
    S: tokio::io::AsyncRead + Unpin,
{
    use tokio::io::AsyncReadExt as _;
    match tokio::time::timeout(STALL_TIMEOUT, stream.read(buf)).await {
        Ok(Ok(n)) => Ok(n),
        Ok(Err(e)) => Err(e.into()),
        Err(_) => Err(crate::error::FetchError::Timeout),
    }
}
