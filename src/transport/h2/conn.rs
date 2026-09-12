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
}

impl H2Conn {
    /// Send client preface with Chrome's exact SETTINGS + connection WINDOW_UPDATE.
    pub async fn start(
        mut stream: SslStream<TcpStream>,
        profile: &BrowserProfile,
    ) -> Result<Self, FetchError> {
        let h2 = &profile.h2;
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
        stream.write_all(&buf).await?;
        stream.flush().await?;
        Ok(Self {
            stream,
            encoder: Encoder::new(),
            decoder: Decoder::new(),
            next_stream: 1,
            conn_window: 65535 + inc as i64,
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

    /// One GET request → full response. Stream-per-request for now (pool later).
    pub async fn get(
        &mut self,
        authority: &str,
        path: &str,
        extra_headers: &[(String, String)],
    ) -> Result<H2Response, FetchError> {
        let stream_id = self.next_stream;
        self.next_stream += 2;

        let mut headers: Vec<(String, String)> = vec![
            (":method".into(), "GET".into()),
            (":authority".into(), authority.into()),
            (":scheme".into(), "https".into()),
            (":path".into(), path.into()),
        ];
        headers.extend(extra_headers.iter().cloned());
        let block = self.encoder.encode(&headers);
        // PRIORITY flag + Chrome's 5-byte priority block, exactly as
        // the real browser sends it on the request HEADERS frame.
        let mut framed = Vec::with_capacity(CHROME_REQ_PRIORITY.len() + block.len());
        framed.extend_from_slice(&CHROME_REQ_PRIORITY);
        framed.extend_from_slice(&block);
        write_frame(
            &mut self.stream,
            HEADERS,
            FLAG_END_HEADERS | FLAG_END_STREAM | FLAG_PRIORITY,
            stream_id,
            &framed,
        )
        .await?;
        self.stream.flush().await?;

        let mut status = 0u16;
        let mut resp_headers: Vec<(String, String)> = Vec::new();
        let mut body: Vec<u8> = Vec::new();
        let mut header_frag: Vec<u8> = Vec::new();
        let initial_window = 6_291_456i64;
        let mut stream_window: i64 = initial_window;
        let mut got_headers = false;
        // RFC 9113: END_STREAM may ride the HEADERS frame that opens
        // the block OR be implied by the response ending at the final
        // CONTINUATION (a bodyless response with a fragmented header
        // block). Only checking the HEADERS frame hung such responses
        // until the 30s response timeout.
        let mut end_stream = false;

        loop {
            let (hdr, payload) = read_frame(&mut self.stream).await?;
            match hdr.ty {
                SETTINGS => {
                    if hdr.flags & FLAG_ACK == 0 {
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
                        // Strip padding/priority fields if flagged.
                        let mut off = 0usize;
                        if hdr.flags & FLAG_PADDED != 0 && !payload.is_empty() {
                            off = payload[0] as usize + 1;
                        }
                        if hdr.flags & FLAG_PRIORITY != 0 {
                            off += 5;
                        }
                        header_frag = payload.get(off..).unwrap_or(&[]).to_vec();
                        // The block cap below guards CONTINUATION
                        // accumulation; an unfragmented HEADERS frame
                        // is bounded only by the 1 MiB frame cap, so
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
                    if hdr.flags & FLAG_END_HEADERS != 0 {
                        let decoded = self.decoder.decode(&header_frag)?;
                        for (n, v) in decoded {
                            // HTTP/2 field names are lowercase; a
                            // nonconforming peer's mixed-case name
                            // must not dodge the case-sensitive
                            // content-encoding/alt-svc lookups
                            // downstream. Normalize like the h1 reader.
                            let n = n.to_ascii_lowercase();
                            // RFC 9113 §8.2.2: CR/LF in field values is
                            // malformed. Such a value must never reach the
                            // cookie jar : it would split later h1 requests.
                            if !crate::fetch::guards::valid_header_value(&n)
                                || !crate::fetch::guards::valid_header_value(&v)
                            {
                                return Err(FetchError::Http(
                                    "h2: header name/value contains CR/LF/NUL : malformed".into(),
                                ));
                            }
                            if n == ":status" {
                                // Status 0 never exists on the wire:
                                // missing or unparseable :status is a
                                // malformed response, not a success
                                // with a nonsense code.
                                status = v.parse().unwrap_or(0);
                                if status == 0 {
                                    return Err(FetchError::Http("h2: unparseable :status".into()));
                                }
                            } else if !n.starts_with(':') {
                                resp_headers.push((n, v));
                            }
                        }
                        got_headers = true;
                        header_frag.clear();
                        if end_stream {
                            break;
                        }
                    }
                }
                DATA if hdr.stream_id == stream_id => {
                    let data = if hdr.flags & FLAG_PADDED != 0 && !payload.is_empty() {
                        let pad = payload[0] as usize;
                        let end = payload.len().saturating_sub(pad);
                        payload.get(1..end).unwrap_or(&[])
                    } else {
                        &payload[..]
                    };
                    body.extend_from_slice(data);
                    if body.len() > MAX_BODY {
                        return Err(FetchError::Http("h2: response body exceeds cap".into()));
                    }
                    stream_window -= data.len() as i64;
                    self.conn_window -= data.len() as i64;
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
                    if self.conn_window < 15_000_000 / 2 {
                        let inc = (15_000_000 - self.conn_window) as u32;
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
                    return Err(FetchError::Http("h2 goaway".into()));
                }
                PUSH_PROMISE => {
                    // RFC 7540 §6.4: RST_STREAM's payload is a 4-byte
                    // error code, not a stream id (this used to send
                    // `stream_id`'s own bytes. We advertise
                    // ENABLE_PUSH=0 in SETTINGS, so a conforming
                    // server never pushes; REFUSED_STREAM tells a
                    // non-conforming one plainly why this is refused.
                    const REFUSED_STREAM: u32 = 0x7;
                    write_frame(
                        &mut self.stream,
                        RST_STREAM,
                        0,
                        hdr.stream_id,
                        &REFUSED_STREAM.to_be_bytes(),
                    )
                    .await
                    .ok();
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
        Ok(H2Response {
            status,
            headers: resp_headers,
            body,
        })
    }
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

    /// Pseudo-header order: m,a,s,p : Chromium's header order.
    #[test]
    fn pseudo_header_order_is_chrome() {
        // Mirrors the order in H2Conn::get; keep both in lockstep.
        let order = [":method", ":authority", ":scheme", ":path"];
        assert_eq!(order, [":method", ":authority", ":scheme", ":path"]);
    }
}
