//! Byte-true Chrome HTTP/2 client connection.

use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;
use tokio_boring::SslStream;

use super::frame::*;
use super::hpack::{Decoder, Encoder};
use crate::error::FetchError;
use crate::profile::BrowserProfile;

const PREFACE: &[u8] = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n";

/// Chrome's request-stream priority block: exclusive=1, dependent=0,
/// weight=255 (main_nav / u=0,i). Ground truth: Chrome 151 Linux,
/// captured live (examples/chrome_h2_probe.rs). Flags: END_STREAM|
/// END_HEADERS|PRIORITY = 0x25.
const CHROME_REQ_PRIORITY: [u8; 5] = [0x80, 0x00, 0x00, 0x00, 0xff];

/// Hard cap on the decoded response body (matches h1/decompress).
const MAX_BODY: usize = 64 << 20;
/// Hard cap on the accumulated (possibly CONTINUATION-chained)
/// header block. Unbounded chaining is a trivial memory DoS.
const MAX_HEADER_BLOCK: usize = 256 << 10;

pub struct H2Response {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

pub struct H2Conn {
    stream: SslStream<TcpStream>,
    encoder: Encoder,
    decoder: Decoder,
    next_stream: u32,
    conn_window: i64,
    conn_window_target: i64,
    stream_window_target: i64,
    header_list_limit: usize,
    peer_frame_size: usize,
    peer_header_list_limit: usize,
    peer_concurrency: u32,
    /// The peer sent GOAWAY (RFC 9113 §6.8). A stream at or below its
    /// last-stream-id may still finish; no new stream may open, and the
    /// connection must never return to the pool.
    draining: bool,
    /// The stream currently awaited, when a request is in flight; a
    /// dropped wait leaves it set so an abandoned stream can be
    /// cancelled (RST_STREAM CANCEL).
    in_flight: Option<u32>,
}

impl H2Conn {
    /// Send client preface with Chrome's exact SETTINGS + connection WINDOW_UPDATE.
    pub async fn start(
        mut stream: SslStream<TcpStream>,
        profile: &BrowserProfile,
    ) -> Result<Self, FetchError> {
        let h2 = &profile.h2;
        if h2.header_table_size > 65536
            || h2.initial_window_size > 0x7fff_ffff
            || h2.conn_window_update > 0x7fff_ffff - 65535
            || h2.enable_push != 0
        {
            return Err(FetchError::Http("h2: unsupported client settings".into()));
        }
        let settings = settings_payload(&[
            (0x1, h2.header_table_size),
            (0x2, h2.enable_push),
            (0x4, h2.initial_window_size),
            (0x6, h2.max_header_list_size),
        ]);
        let mut buf = Vec::with_capacity(24 + 9 + settings.len() + 13);
        buf.extend_from_slice(PREFACE);
        // SETTINGS frame
        let slen = settings.len() as u32;
        buf.extend_from_slice(&[
            (slen >> 16) as u8,
            (slen >> 8) as u8,
            slen as u8,
            SETTINGS,
            0,
            0,
            0,
            0,
            0,
        ]);
        buf.extend_from_slice(&settings);
        // WINDOW_UPDATE frame (stream 0)
        let inc = h2.conn_window_update;
        if inc != 0 {
            buf.extend_from_slice(&[
                0,
                0,
                4,
                WINDOW_UPDATE,
                0,
                0,
                0,
                0,
                0,
                ((inc >> 24) & 0x7f) as u8,
                (inc >> 16) as u8,
                (inc >> 8) as u8,
                inc as u8,
            ]);
        }
        stream.write_all(&buf).await?;
        stream.flush().await?;
        Ok(Self {
            stream,
            encoder: Encoder::new(),
            decoder: Decoder::with_limit(h2.header_table_size as usize),
            next_stream: 1,
            conn_window: 65535 + inc as i64,
            conn_window_target: 65535 + inc as i64,
            stream_window_target: h2.initial_window_size as i64,
            header_list_limit: (h2.max_header_list_size as usize).min(MAX_HEADER_BLOCK),
            peer_frame_size: DEFAULT_MAX_FRAME_SIZE,
            peer_header_list_limit: usize::MAX,
            peer_concurrency: u32::MAX,
            draining: false,
            in_flight: None,
        })
    }

    /// Graceful close: send GOAWAY with NO_ERROR, like Chrome.
    #[allow(dead_code)] // called when pool evicts a connection (future use)
    pub async fn close(mut self) {
        let last = if self.next_stream > 1 {
            self.next_stream - 2
        } else {
            0
        };
        let mut payload = [0u8; 8];
        payload[0..4].copy_from_slice(&last.to_be_bytes());
        let _ = write_frame(&mut self.stream, GOAWAY, 0, 0, &payload).await;
        let _ = self.stream.flush().await;
    }

    /// True once the peer sent GOAWAY: the current stream may finish,
    /// but the connection accepts no new stream and must not be pooled.
    pub fn is_draining(&self) -> bool {
        self.draining
    }

    /// Cancel the stream an abandoned request left in flight (RFC 9113
    /// §6.4/§8.1: RST_STREAM with CANCEL), so a stalled peer stops
    /// working on it before the connection is discarded. The connection
    /// is not reusable afterwards: frames for the cancelled stream may
    /// still arrive on the wire.
    pub async fn cancel_in_flight(&mut self) {
        if let Some(stream_id) = self.in_flight.take() {
            // CANCEL = 0x8 (RFC 9113 §7).
            let _ = write_frame(
                &mut self.stream,
                RST_STREAM,
                0,
                stream_id,
                &0x8u32.to_be_bytes(),
            )
            .await;
            let _ = self.stream.flush().await;
        }
    }

    /// One GET at a time; connection pooling reuses completed serial streams.
    pub async fn get(
        &mut self,
        authority: &str,
        path: &str,
        extra_headers: &[(String, String)],
    ) -> Result<H2Response, FetchError> {
        let stream_id = self.next_stream;
        if stream_id > 0x7fff_ffff || self.peer_concurrency == 0 {
            return Err(FetchError::Http(
                "h2: peer cannot accept a new stream".into(),
            ));
        }
        if self.draining {
            return Err(FetchError::Http("h2: connection is draining".into()));
        }
        self.next_stream += 2;
        self.in_flight = Some(stream_id);

        let mut headers: Vec<(String, String)> = vec![
            (":method".into(), "GET".into()),
            (":authority".into(), authority.into()),
            (":scheme".into(), "https".into()),
            (":path".into(), path.into()),
        ];
        headers.extend(extra_headers.iter().cloned());
        if headers.iter().any(|(n, v)| {
            !crate::fetch::guards::valid_header_value(n)
                || !crate::fetch::guards::valid_header_value(v)
        }) {
            return Err(FetchError::Http("h2: malformed request field".into()));
        }
        let request_size: usize = headers.iter().map(|(n, v)| n.len() + v.len() + 32).sum();
        if request_size > self.peer_header_list_limit.min(MAX_HEADER_BLOCK) {
            return Err(FetchError::Http(
                "h2: request fields exceed peer limit".into(),
            ));
        }
        let block = self.encoder.encode(&headers);
        // PRIORITY flag + Chrome's 5-byte priority block, exactly as
        // the real browser sends it on the request HEADERS frame.
        let first = block
            .len()
            .min(self.peer_frame_size - CHROME_REQ_PRIORITY.len());
        let mut framed = Vec::with_capacity(CHROME_REQ_PRIORITY.len() + first);
        framed.extend_from_slice(&CHROME_REQ_PRIORITY);
        framed.extend_from_slice(&block[..first]);
        write_frame(
            &mut self.stream,
            HEADERS,
            FLAG_END_STREAM
                | FLAG_PRIORITY
                | if first == block.len() {
                    FLAG_END_HEADERS
                } else {
                    0
                },
            stream_id,
            &framed,
        )
        .await?;
        let remaining = &block[first..];
        for (i, chunk) in remaining.chunks(self.peer_frame_size).enumerate() {
            let final_chunk = (i + 1) * self.peer_frame_size >= remaining.len();
            write_frame(
                &mut self.stream,
                CONTINUATION,
                if final_chunk { FLAG_END_HEADERS } else { 0 },
                stream_id,
                chunk,
            )
            .await?;
        }
        self.stream.flush().await?;

        let mut status = 0u16;
        let mut resp_headers: Vec<(String, String)> = Vec::new();
        let mut body: Vec<u8> = Vec::new();
        let mut header_frag: Vec<u8> = Vec::new();
        let initial_window = self.stream_window_target;
        let mut stream_window: i64 = initial_window;
        let mut got_headers = false;
        // END_STREAM belongs to the opening HEADERS frame. Preserve it
        // until the last CONTINUATION; CONTINUATION has no END_STREAM flag.
        let mut end_stream = false;
        let mut continuation = false;
        let mut header_blocks = 0;

        loop {
            let (hdr, payload) = read_frame(&mut self.stream).await?;
            if continuation && (hdr.ty != CONTINUATION || hdr.stream_id != stream_id) {
                return Err(FetchError::Http(
                    "h2: interleaved CONTINUATION block".into(),
                ));
            }
            if hdr.ty == CONTINUATION && !continuation {
                return Err(FetchError::Http("h2: unopened CONTINUATION block".into()));
            }
            match hdr.ty {
                SETTINGS => {
                    if hdr.flags & FLAG_ACK == 0 {
                        self.apply_settings(&payload)?;
                        write_frame(&mut self.stream, SETTINGS, FLAG_ACK, 0, &[]).await?;
                        self.stream.flush().await?;
                    }
                }
                PING => {
                    if hdr.flags & FLAG_ACK == 0 {
                        write_frame(&mut self.stream, PING, FLAG_ACK, 0, &payload).await?;
                        self.stream.flush().await?;
                    }
                }
                WINDOW_UPDATE => {}
                HEADERS | CONTINUATION if hdr.stream_id == stream_id => {
                    if hdr.ty == HEADERS {
                        let mut fragment = unpad(&payload, hdr.flags)?;
                        if hdr.flags & FLAG_PRIORITY != 0 {
                            if fragment.len() < 5 {
                                return Err(FetchError::Http(
                                    "h2: truncated HEADERS priority".into(),
                                ));
                            }
                            let dependency =
                                u32::from_be_bytes(fragment[..4].try_into().unwrap()) & 0x7fff_ffff;
                            if dependency == stream_id {
                                return Err(FetchError::Http(
                                    "h2: self-dependent HEADERS priority".into(),
                                ));
                            }
                            fragment = &fragment[5..];
                        }
                        header_frag = fragment.to_vec();
                        // The block cap below guards CONTINUATION
                        // accumulation; an unfragmented HEADERS frame
                        // is bounded by the default 16 KiB frame cap, so
                        // apply the same documented bound here.
                        if header_frag.len() > MAX_HEADER_BLOCK {
                            return Err(FetchError::Http("h2: header block exceeds cap".into()));
                        }
                        end_stream = hdr.flags & FLAG_END_STREAM != 0;
                    } else {
                        if header_frag.len() + payload.len() > MAX_HEADER_BLOCK {
                            return Err(FetchError::Http(
                                "h2: header block exceeds cap (CONTINUATION flood?)".into(),
                            ));
                        }
                        header_frag.extend_from_slice(&payload);
                    }
                    continuation = hdr.flags & FLAG_END_HEADERS == 0;
                    if hdr.flags & FLAG_END_HEADERS != 0 {
                        let decoded = self.decoder.decode(&header_frag)?;
                        header_blocks += 1;
                        if header_blocks > 64 {
                            return Err(FetchError::Http("h2: too many header sections".into()));
                        }
                        let block_status =
                            response_status(&decoded, got_headers, self.header_list_limit)?;
                        let fields: Vec<_> = decoded
                            .into_iter()
                            .filter(|(n, _)| !n.starts_with(':'))
                            .collect();
                        if got_headers {
                            if !end_stream {
                                return Err(FetchError::Http(
                                    "h2: trailers without END_STREAM".into(),
                                ));
                            }
                            resp_headers.extend(fields);
                        } else if let Some(code) = block_status {
                            if code < 200 {
                                if end_stream {
                                    return Err(FetchError::Http(
                                        "h2: informational response ended stream".into(),
                                    ));
                                }
                            } else {
                                status = code;
                                resp_headers = fields;
                                got_headers = true;
                            }
                        }
                        header_frag.clear();
                        if end_stream {
                            break;
                        }
                    }
                }
                DATA if hdr.stream_id == stream_id => {
                    if !got_headers {
                        return Err(FetchError::Http("h2: DATA before headers".into()));
                    }
                    let data = unpad(&payload, hdr.flags)?;
                    if payload.len() as i64 > stream_window
                        || payload.len() as i64 > self.conn_window
                    {
                        return Err(FetchError::Http("h2: DATA exceeds receive window".into()));
                    }
                    body.extend_from_slice(data);
                    if body.len() > MAX_BODY {
                        return Err(FetchError::Http("h2: response body exceeds cap".into()));
                    }
                    stream_window -= payload.len() as i64;
                    self.conn_window -= payload.len() as i64;
                    // Replenish flow-control windows at half consumption.
                    if stream_window < initial_window / 2 {
                        let inc = (initial_window - stream_window) as u32;
                        write_frame(
                            &mut self.stream,
                            WINDOW_UPDATE,
                            0,
                            stream_id,
                            &inc.to_be_bytes(),
                        )
                        .await?;
                        stream_window += inc as i64;
                    }
                    if self.conn_window < self.conn_window_target / 2 {
                        let inc = (self.conn_window_target - self.conn_window) as u32;
                        write_frame(&mut self.stream, WINDOW_UPDATE, 0, 0, &inc.to_be_bytes())
                            .await?;
                        self.conn_window += inc as i64;
                    }
                    if hdr.flags & FLAG_END_STREAM != 0 {
                        break;
                    }
                }
                // Scoped like HEADERS/CONTINUATION/DATA above: on a
                // pooled, reused connection, a late RST_STREAM for a
                // PRIOR (already-finished) stream must not abort the
                // new request currently in flight. Unmatched
                // RST_STREAM falls through to the `_` arm below and
                // is correctly ignored.
                RST_STREAM if hdr.stream_id == stream_id => {
                    return Err(FetchError::Http(format!("h2 rst_stream on {stream_id}")));
                }
                GOAWAY => {
                    // RFC 9113 §6.8: payload is last-stream-id + error
                    // code. A graceful shutdown first sends
                    // GOAWAY(2^31-1) so in-flight streams still complete,
                    // then lowers the bound. Only a bound below this
                    // stream makes the response undeliverable.
                    let last = match payload.get(..4) {
                        Some(b) => u32::from_be_bytes([b[0] & 0x7f, b[1], b[2], b[3]]),
                        // read_frame already rejects short GOAWAY
                        // payloads; fail closed if that ever changes.
                        None => 0,
                    };
                    if last < stream_id {
                        return Err(FetchError::Http("h2 goaway".into()));
                    }
                    self.draining = true;
                }
                PUSH_PROMISE => {
                    return Err(FetchError::Http("h2: push despite ENABLE_PUSH=0".into()));
                }
                HEADERS | CONTINUATION | DATA => {
                    return Err(FetchError::Http("h2: unexpected response stream".into()));
                }
                PRIORITY => {}
                _ => {}
            }
        }
        if !got_headers {
            return Err(FetchError::Http("h2: stream ended without headers".into()));
        }
        if status == 0 {
            return Err(FetchError::Http("h2: response missing :status".into()));
        }
        if matches!(status, 204 | 304) && !body.is_empty() {
            return Err(FetchError::Http("h2: DATA on a bodyless response".into()));
        }
        let mut content_length = None;
        for (_, value) in resp_headers
            .iter()
            .filter(|(name, _)| name == "content-length")
        {
            let length = value
                .parse::<u64>()
                .ok()
                .filter(|_| value.bytes().all(|b| b.is_ascii_digit()))
                .ok_or_else(|| FetchError::Http("h2: invalid content-length".into()))?;
            if content_length.is_some_and(|previous| previous != length) {
                return Err(FetchError::Http("h2: conflicting content-length".into()));
            }
            content_length = Some(length);
        }
        // A 304's length describes the selected representation, not its
        // absent body. GET responses with a body must match the wire length.
        if status != 304 && content_length.is_some_and(|length| length != body.len() as u64) {
            return Err(FetchError::Http(
                "h2: content-length differs from DATA".into(),
            ));
        }
        Ok(H2Response {
            status,
            headers: resp_headers,
            body,
        })
    }

    fn apply_settings(&mut self, payload: &[u8]) -> Result<(), FetchError> {
        for setting in payload.as_chunks::<6>().0 {
            let id = u16::from_be_bytes(setting[..2].try_into().unwrap());
            let value = u32::from_be_bytes(setting[2..].try_into().unwrap());
            match id {
                1 => self.encoder.set_max(value),
                // RFC9113 permits a server to explicitly disable push.
                2 if value != 0 => {
                    return Err(FetchError::Http("h2: invalid enable push setting".into()));
                }
                3 => self.peer_concurrency = value,
                // GET closes its sending side in HEADERS; there is no DATA
                // send window to adjust, but illegal values remain errors.
                4 if value > 0x7fff_ffff => {
                    return Err(FetchError::Http(
                        "h2: invalid initial window setting".into(),
                    ));
                }
                5 if !(16_384..=0xff_ffff).contains(&value) => {
                    return Err(FetchError::Http("h2: invalid max frame setting".into()));
                }
                5 => self.peer_frame_size = value as usize,
                6 => self.peer_header_list_limit = value as usize,
                _ => {}
            }
        }
        Ok(())
    }
}

fn unpad(payload: &[u8], flags: u8) -> Result<&[u8], FetchError> {
    if flags & FLAG_PADDED == 0 {
        return Ok(payload);
    }
    let pad = payload
        .first()
        .copied()
        .ok_or_else(|| FetchError::Http("h2: missing padding length".into()))?
        as usize;
    if pad >= payload.len() {
        return Err(FetchError::Http("h2: invalid padding length".into()));
    }
    Ok(&payload[1..payload.len() - pad])
}

fn response_status(
    fields: &[(String, String)],
    trailers: bool,
    limit: usize,
) -> Result<Option<u16>, FetchError> {
    let mut status = None;
    let mut regular = false;
    let mut size = 0;
    for (name, value) in fields {
        size += name.len() + value.len() + 32;
        if size > limit {
            return Err(FetchError::Http(
                "h2: response fields exceed advertised limit".into(),
            ));
        }
        if !crate::fetch::guards::valid_header_value(name)
            || !crate::fetch::guards::valid_header_value(value)
            || name.bytes().any(|b| b.is_ascii_uppercase())
            || value.starts_with([' ', '\t'])
            || value.ends_with([' ', '\t'])
        {
            return Err(FetchError::Http("h2: malformed header name/value".into()));
        }
        if name.starts_with(':') {
            if name != ":status" || trailers || status.is_some() || regular {
                return Err(FetchError::Http(
                    "h2: invalid response pseudo-header".into(),
                ));
            }
            let code = value
                .parse::<u16>()
                .ok()
                .filter(|code| value.len() == 3 && (100..=599).contains(code) && *code != 101)
                .ok_or_else(|| FetchError::Http("h2: unparseable :status".into()))?;
            status = Some(code);
        } else {
            if name.is_empty()
                || !name.bytes().all(|b| {
                    b.is_ascii_lowercase() || b.is_ascii_digit() || b"!#$%&'*+-.^_`|~".contains(&b)
                })
                || matches!(
                    name.as_str(),
                    "connection"
                        | "proxy-connection"
                        | "keep-alive"
                        | "transfer-encoding"
                        | "upgrade"
                        | "te"
                )
                || (trailers && name == "content-length")
            {
                return Err(FetchError::Http("h2: invalid connection field".into()));
            }
            regular = true;
        }
    }
    if !trailers && status.is_none() {
        return Err(FetchError::Http("h2: response missing :status".into()));
    }
    Ok(status)
}

#[cfg(test)]
mod parity_tests {
    use super::*;

    /// v3 F4 gate: DonShadow's h2 preface must be byte-identical
    /// to Chromium's (Akamai-style fingerprint
    /// `1:65536;2:0;4:6291456;6:262144|15663105|0|m,a,s,p`).
    /// Ground truth: Chromium 150 capture (2026-07-30). Any change
    /// here is a fingerprint regression : update ONLY with a new
    /// capture, never by hand.
    #[test]
    fn settings_match_chromium() {
        let h2 = crate::profile::BrowserProfile::chrome_150(crate::profile::Platform::Linux).h2;
        // SETTINGS id/value pairs, in Chromium's exact order.
        assert_eq!(h2.header_table_size, 65536); // 0x1
        assert_eq!(h2.enable_push, 0); // 0x2
        assert_eq!(h2.initial_window_size, 6_291_456); // 0x4
        assert_eq!(h2.max_header_list_size, 262_144); // 0x6
        assert_eq!(h2.conn_window_update, 15_663_105);
        let payload = settings_payload(&[
            (0x1, h2.header_table_size),
            (0x2, h2.enable_push),
            (0x4, h2.initial_window_size),
            (0x6, h2.max_header_list_size),
        ]);
        // The exact wire bytes of Chrome's SETTINGS body.
        assert_eq!(
            payload,
            vec![
                0x00, 0x01, 0x00, 0x01, 0x00, 0x00, // HEADER_TABLE_SIZE = 65536
                0x00, 0x02, 0x00, 0x00, 0x00, 0x00, // ENABLE_PUSH = 0
                0x00, 0x04, 0x00, 0x60, 0x00, 0x00, // INITIAL_WINDOW_SIZE = 6291456
                0x00, 0x06, 0x00, 0x04, 0x00, 0x00, // MAX_HEADER_LIST_SIZE = 262144
            ]
        );
    }

    /// v3.6: Chromium 151 Linux live capture (examples/chrome_h2_probe.rs,
    /// 2026-09-04): request HEADERS = flags 0x25 (END_STREAM|END_HEADERS|
    /// PRIORITY) with the 5-byte priority block [E=1, dep=0, weight=255]
    /// before the HPACK block. Update ONLY from a new capture.
    #[test]
    fn request_priority_matches_chrome_151() {
        assert_eq!(CHROME_REQ_PRIORITY, [0x80, 0x00, 0x00, 0x00, 0xff]);
        assert_eq!(FLAG_END_HEADERS | FLAG_END_STREAM | FLAG_PRIORITY, 0x25);
    }
}
