//! Minimal HTTP/1.1 client with Chrome's exact header order (fallback for
//! origins without h2).

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::error::FetchError;

pub struct H1Response {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

use super::MAX_BODY;

/// Cap on any framing line the client accumulates while looking
/// for its terminator: the header block, a chunk-size line, the
/// trailer section. A server that never sends the CRLF must run
/// into this, not into the allocator.
const MAX_LINE: usize = 1 << 20;
/// Cap on interim (1xx) responses skipped before the final one.
/// Real servers send at most one or two (100 and/or 103).
const MAX_INTERIM: usize = 8;

/// Generic over any async stream : works for both TLS
/// (`SslStream<TcpStream>`) and raw plaintext `TcpStream`
/// (the http:// path).
pub async fn get<S>(
    stream: &mut S,
    path: &str,
    headers: &[(String, String)],
) -> Result<H1Response, FetchError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    // Header values are synthesized partly from response data
    // (cookies). A CR/LF/NUL inside one would split the request
    // on the wire : refuse to send instead.
    for (n, v) in headers {
        if !crate::fetch::guards::valid_header_value(n)
            || !crate::fetch::guards::valid_header_value(v)
        {
            return Err(FetchError::Http(
                "h1: invalid header value (CR/LF/NUL) : refused to send".into(),
            ));
        }
    }
    let mut req = format!("GET {path} HTTP/1.1\r\n");
    for (n, v) in headers {
        req.push_str(n);
        req.push_str(": ");
        req.push_str(v);
        req.push_str("\r\n");
    }
    req.push_str("\r\n");
    stream.write_all(req.as_bytes()).await?;
    stream.flush().await?;

    // Read header blocks until a FINAL status arrives. Interim (1xx)
    // responses precede the real one on the same connection : 100
    // Continue, and 103 Early Hints from CDNs (Cloudflare, Fastly)
    // for any page with preload hints. An interim block carries no
    // body and its headers are advisory : discard it and keep
    // parsing. Taking the first status line as THE response handed
    // callers a 103 with the real response bytes as an unframed
    // "read to close" body.
    let mut buf: Vec<u8> = Vec::with_capacity(16384);
    let mut tmp = [0u8; 16384];
    let mut interim = 0usize;
    let (status, headers_out, header_end) = loop {
        let header_end = loop {
            if let Some(pos) = find(&buf, b"\r\n\r\n") {
                break pos + 4;
            }
            if buf.len() > MAX_LINE {
                return Err(FetchError::Http("h1: header block too large".into()));
            }
            let n = stream.read(&mut tmp).await?;
            if n == 0 {
                return Err(FetchError::Http("h1: eof before headers".into()));
            }
            buf.extend_from_slice(&tmp[..n]);
        };

        let head = String::from_utf8_lossy(&buf[..header_end]);
        let mut lines = head.lines();
        let status_line = lines.next().unwrap_or("");
        let status: u16 = status_line
            .split_whitespace()
            .nth(1)
            .and_then(|s| s.parse().ok())
            .ok_or_else(|| FetchError::Http(format!("h1: bad status line: {status_line}")))?;
        if (100..200).contains(&status) {
            // 101 means the server thinks it negotiated an upgrade
            // this client never requested : the framing after it is
            // not HTTP/1.1, bail instead of misparsing it.
            if status == 101 {
                return Err(FetchError::Http(
                    "h1: unexpected 101 switching protocols (no upgrade requested)".into(),
                ));
            }
            interim += 1;
            // A server streaming 1xx forever must hit a counter,
            // not the response timeout.
            if interim > MAX_INTERIM {
                return Err(FetchError::Http(format!(
                    "h1: more than {MAX_INTERIM} interim responses"
                )));
            }
            buf.drain(..header_end);
            continue;
        }
        let mut headers_out = Vec::new();
        for line in lines {
            if let Some((n, v)) = line.split_once(':') {
                headers_out.push((n.trim().to_ascii_lowercase(), v.trim().to_string()));
            }
        }
        break (status, headers_out, header_end);
    };

    let mut body = buf[header_end..].to_vec();
    let is_chunked = headers_out
        .iter()
        .any(|(n, v)| n == "transfer-encoding" && v.contains("chunked"));
    // RFC 9112 6.3: a message with multiple, different Content-Length
    // values is invalid (request-smuggling class); browsers reject it.
    // Same value repeated is tolerated. Parse failure on ANY of them is
    // also invalid per RFC; stay lenient there (treat as absent) but
    // reject the conflicting-values case outright.
    let mut content_lens: Vec<usize> = headers_out
        .iter()
        .filter(|(n, _)| n == "content-length")
        .filter_map(|(_, v)| v.trim().parse().ok())
        .collect();
    content_lens.sort_unstable();
    content_lens.dedup();
    let content_len = match content_lens.as_slice() {
        [] => None,
        [one] => Some(*one),
        conflicting => {
            return Err(FetchError::Http(format!(
                "h1: conflicting content-length headers: {conflicting:?}"
            )));
        }
    };

    if is_chunked {
        body = read_chunked(stream, body).await?;
    } else if let Some(cl) = content_len {
        // A lying Content-Length must not turn into a giant alloc.
        if cl > MAX_BODY {
            return Err(FetchError::Http(format!(
                "h1: content-length {cl} exceeds body cap"
            )));
        }
        while body.len() < cl {
            let n = stream.read(&mut tmp).await?;
            if n == 0 {
                break;
            }
            body.extend_from_slice(&tmp[..n]);
        }
        body.truncate(cl);
    } else {
        // Read to close : still capped.
        loop {
            let n = stream.read(&mut tmp).await?;
            if n == 0 {
                break;
            }
            body.extend_from_slice(&tmp[..n]);
            if body.len() > MAX_BODY {
                return Err(FetchError::Http("h1: body exceeds cap".into()));
            }
        }
    }

    Ok(H1Response {
        status,
        headers: headers_out,
        body,
    })
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    // memmem is sublinear (two-way search); the naive windows scan was
    // O(n*m) and re-scanned from byte 0 after every 16 KiB trickle
    // (quadratic on slow-drip responses).
    memchr::memmem::find(hay, needle)
}

/// Decode chunked transfer coding from `prefix` (already-read bytes) + stream.
async fn read_chunked<S>(stream: &mut S, prefix: Vec<u8>) -> Result<Vec<u8>, FetchError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut raw = prefix;
    let mut tmp = [0u8; 16384];
    let mut out = Vec::new();
    loop {
        // Ensure we have a size line. Capped like the header block:
        // a server that never sends the CRLF (or a chunk extension
        // of arbitrary length) must not grow `raw` without bound --
        // the body caps below only apply once a size has parsed, so
        // this loop used to be the one uncapped allocation on the
        // response path, ended only by the response timeout.
        let line_end = loop {
            if let Some(pos) = find(&raw, b"\r\n") {
                break pos;
            }
            if raw.len() > MAX_LINE {
                return Err(FetchError::Http("h1: chunk size line too large".into()));
            }
            let n = stream.read(&mut tmp).await?;
            if n == 0 {
                return Err(FetchError::Http("h1: eof in chunk size".into()));
            }
            raw.extend_from_slice(&tmp[..n]);
        };
        let size_str = String::from_utf8_lossy(&raw[..line_end]);
        let size = usize::from_str_radix(size_str.split(';').next().unwrap_or("").trim(), 16)
            .map_err(|_| FetchError::Http(format!("h1: bad chunk size: {size_str}")))?;
        if size > MAX_BODY {
            return Err(FetchError::Http("h1: chunk size exceeds cap".into()));
        }
        let mut rest = raw.split_off(line_end + 2);
        if size == 0 {
            // Trailer section ends with empty line. Same cap as the
            // header block it structurally is.
            while !rest.starts_with(b"\r\n") {
                if let Some(pos) = find(&rest, b"\r\n\r\n") {
                    rest.truncate(pos + 4);
                    break;
                }
                if rest.len() > MAX_LINE {
                    return Err(FetchError::Http("h1: trailer section too large".into()));
                }
                let n = stream.read(&mut tmp).await?;
                if n == 0 {
                    break;
                }
                rest.extend_from_slice(&tmp[..n]);
            }
            break;
        }
        // Check the running total BEFORE reading the chunk in:
        // checked after, two back-to-back near-cap chunks peaked at
        // `out` + `rest` = 2 x MAX_BODY before the cap fired.
        if out.len() + size > MAX_BODY {
            return Err(FetchError::Http("h1: chunked body exceeds cap".into()));
        }
        while rest.len() < size + 2 {
            let n = stream.read(&mut tmp).await?;
            if n == 0 {
                return Err(FetchError::Http("h1: eof in chunk data".into()));
            }
            rest.extend_from_slice(&tmp[..n]);
        }
        out.extend_from_slice(&rest[..size]);
        raw = rest.split_off(size + 2);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    // RFC 9112 6.3: differing Content-Length values = invalid message
    // (request-smuggling class). Browsers reject; we do too.
    #[tokio::test]
    async fn conflicting_content_length_is_rejected() {
        let wire = b"HTTP/1.1 200 OK\r\ncontent-length: 5\r\ncontent-length: 9\r\n\r\nhello";
        let mut s = RO(&wire[..]);
        match get(&mut s, "/", &[]).await {
            Ok(r) => panic!("must reject conflicting lengths, got status {}", r.status),
            Err(e) => assert!(e.to_string().contains("content-length"), "{e}"),
        }
    }

    // The same value repeated is tolerated (HTTP/1.0 proxies do this).
    #[tokio::test]
    async fn repeated_identical_content_length_is_tolerated() {
        let wire = b"HTTP/1.1 200 OK\r\ncontent-length: 5\r\ncontent-length: 5\r\n\r\nhello";
        let mut s = RO(&wire[..]);
        let resp = get(&mut s, "/", &[]).await.expect("valid message");
        assert_eq!(resp.body, b"hello");
    }
    use super::*;
    use std::pin::Pin;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::task::{Context, Poll};

    /// A stream that serves `head` once, then an endless supply of
    /// `filler` bytes (never EOF). Counts bytes handed out so a test
    /// can assert the reader gave up after a bounded amount, and
    /// fails the read itself past TEST_GUARD so a reader with no cap
    /// fails the test instead of eating the machine.
    struct Endless {
        head: Vec<u8>,
        filler: u8,
        served: Arc<AtomicUsize>,
    }

    const TEST_GUARD: usize = 16 << 20;

    impl AsyncRead for Endless {
        fn poll_read(
            mut self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &mut tokio::io::ReadBuf<'_>,
        ) -> Poll<std::io::Result<()>> {
            if self.served.load(Ordering::Relaxed) > TEST_GUARD {
                return Poll::Ready(Err(std::io::Error::other(
                    "test guard: reader accepted more than TEST_GUARD bytes",
                )));
            }
            let n = buf.remaining();
            if !self.head.is_empty() {
                let take = n.min(self.head.len());
                let chunk: Vec<u8> = self.head.drain(..take).collect();
                buf.put_slice(&chunk);
                self.served.fetch_add(take, Ordering::Relaxed);
            } else {
                let fill = vec![self.filler; n];
                buf.put_slice(&fill);
                self.served.fetch_add(n, Ordering::Relaxed);
            }
            Poll::Ready(Ok(()))
        }
    }

    impl AsyncWrite for Endless {
        fn poll_write(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            data: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            Poll::Ready(Ok(data.len()))
        }
        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    fn endless(head: &[u8], filler: u8) -> (Endless, Arc<AtomicUsize>) {
        let served = Arc::new(AtomicUsize::new(0));
        (
            Endless {
                head: head.to_vec(),
                filler,
                served: served.clone(),
            },
            served,
        )
    }

    // The chunk-size-line loop accumulated bytes until it saw CRLF,
    // with no cap: a server answering `Transfer-Encoding: chunked`
    // with an endless CRLF-free stream made the client allocate
    // without bound (only the 30s response timeout ended it). The
    // header-block read a few lines up has a 1 MiB cap; this loop
    // simply lacked it.
    #[tokio::test]
    async fn chunk_size_line_without_crlf_is_capped() {
        let (mut s, served) = endless(b"", b'a');
        let err = read_chunked(&mut s, Vec::new())
            .await
            .expect_err("must give up");
        assert!(err.to_string().contains("chunk size line"), "{err}");
        assert!(
            served.load(Ordering::Relaxed) <= MAX_LINE + 2 * 16384,
            "read {} bytes before giving up",
            served.load(Ordering::Relaxed)
        );
    }

    // Same omission in the trailer loop after the terminating 0-chunk.
    #[tokio::test]
    async fn endless_trailers_are_capped() {
        let (mut s, served) = endless(b"0\r\nX-Trailer: ", b'x');
        let err = read_chunked(&mut s, Vec::new())
            .await
            .expect_err("must give up");
        assert!(err.to_string().contains("trailer"), "{err}");
        assert!(served.load(Ordering::Relaxed) <= MAX_LINE + 2 * 16384);
    }

    // A well-formed body, with a chunk extension and a trailer, still
    // decodes and consumes exactly what it should.
    #[tokio::test]
    async fn well_formed_chunked_body_decodes() {
        let wire = b"5;ext=1\r\nhello\r\n6\r\n world\r\n0\r\nX-Sum: abc\r\n\r\n";
        let mut s = tokio::io::BufReader::new(&wire[..]);
        let mut ro = RO(&mut s);
        let out = read_chunked(&mut ro, Vec::new()).await.expect("decode");
        assert_eq!(out, b"hello world");
    }

    /// Read-only stream adapter: forwards reads, accepts (discards)
    /// writes. Lets a byte slice stand in for a server connection.
    struct RO<R>(R);
    impl<R: AsyncRead + Unpin> AsyncRead for RO<R> {
        fn poll_read(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &mut tokio::io::ReadBuf<'_>,
        ) -> Poll<std::io::Result<()>> {
            Pin::new(&mut self.0).poll_read(cx, buf)
        }
    }
    impl<R: Unpin> AsyncWrite for RO<R> {
        fn poll_write(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            d: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            Poll::Ready(Ok(d.len()))
        }
        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    // Interim responses (100 Continue, 103 Early Hints) precede the
    // final response on the same connection : Cloudflare and Fastly
    // send 103 for any page with preload hints. The parser took the
    // first status line as THE response: callers got status 103, the
    // hint headers, and (with no framing headers on a 103) a "read
    // to close" body : the real response, raw, after a 30s hang on
    // keep-alive connections.
    #[tokio::test]
    async fn interim_responses_are_skipped() {
        let wire = b"HTTP/1.1 103 Early Hints\r\nlink: </s.css>; rel=preload\r\n\r\n\
                     HTTP/1.1 100 Continue\r\n\r\n\
                     HTTP/1.1 200 OK\r\ncontent-length: 5\r\n\r\nhello";
        let mut s = RO(&wire[..]);
        let resp = get(&mut s, "/", &[]).await.expect("fetch");
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body, b"hello");
        assert!(
            resp.headers
                .iter()
                .any(|(n, v)| n == "content-length" && v == "5"),
            "final headers, not the 103's: {:?}",
            resp.headers
        );
        // Exactly the final block's headers: the 103's link hint
        // must not leak through, and a drain that nibbled into the
        // next block would corrupt this set.
        assert_eq!(resp.headers.len(), 1, "{:?}", resp.headers);
    }

    // A server streaming 1xx blocks forever must hit a counter, not
    // spin until the response timeout.
    #[tokio::test]
    async fn endless_interim_responses_are_refused() {
        let wire = b"HTTP/1.1 103 Early Hints\r\n\r\n".repeat(50);
        let mut s = RO(&wire[..]);
        let err = match get(&mut s, "/", &[]).await {
            Ok(r) => panic!("must give up, got status {}", r.status),
            Err(e) => e,
        };
        assert!(err.to_string().contains("interim"), "{err}");
    }
}
