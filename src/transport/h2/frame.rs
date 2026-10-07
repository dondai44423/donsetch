//! HTTP/2 frame layer (RFC 7540 §4-6). Minimal client subset.

use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::error::FetchError;

pub const DATA: u8 = 0x0;
pub const HEADERS: u8 = 0x1;
pub const PRIORITY: u8 = 0x2;
pub const RST_STREAM: u8 = 0x3;
pub const SETTINGS: u8 = 0x4;
pub const PUSH_PROMISE: u8 = 0x5;
pub const PING: u8 = 0x6;
pub const GOAWAY: u8 = 0x7;
pub const WINDOW_UPDATE: u8 = 0x8;
pub const CONTINUATION: u8 = 0x9;

pub const FLAG_END_STREAM: u8 = 0x1;
pub const FLAG_ACK: u8 = 0x1;
pub const FLAG_END_HEADERS: u8 = 0x4;
pub const FLAG_PADDED: u8 = 0x8;
pub const FLAG_PRIORITY: u8 = 0x20;

// We do not advertise SETTINGS_MAX_FRAME_SIZE, so the peer's send limit
// stays at the RFC default. Its own setting governs our writes, not reads.
pub const DEFAULT_MAX_FRAME_SIZE: usize = 16_384;

#[derive(Debug, Clone, Copy)]
pub struct FrameHeader {
    pub ty: u8,
    pub flags: u8,
    pub stream_id: u32,
}

pub async fn read_frame<R: AsyncReadExt + Unpin>(
    r: &mut R,
) -> Result<(FrameHeader, Vec<u8>), FetchError> {
    let mut hdr = [0u8; 9];
    r.read_exact(&mut hdr).await?;
    let len = u32::from_be_bytes([0, hdr[0], hdr[1], hdr[2]]);
    if len as usize > DEFAULT_MAX_FRAME_SIZE {
        return Err(FetchError::Http(format!("h2 frame too large: {len}")));
    }
    let header = FrameHeader {
        ty: hdr[3],
        flags: hdr[4],
        stream_id: u32::from_be_bytes([hdr[5] & 0x7f, hdr[6], hdr[7], hdr[8]]),
    };
    let invalid = match header.ty {
        DATA | HEADERS | PUSH_PROMISE | CONTINUATION => header.stream_id == 0,
        PRIORITY => header.stream_id == 0 || len != 5,
        RST_STREAM => header.stream_id == 0 || len != 4,
        SETTINGS => {
            header.stream_id != 0
                || if header.flags & FLAG_ACK != 0 {
                    len != 0
                } else {
                    !len.is_multiple_of(6)
                }
        }
        PING => header.stream_id != 0 || len != 8,
        GOAWAY => header.stream_id != 0 || len < 8,
        WINDOW_UPDATE => len != 4,
        _ => false,
    };
    if invalid {
        return Err(FetchError::Http(format!(
            "h2: invalid frame type={} stream={} length={len}",
            header.ty, header.stream_id
        )));
    }
    let mut payload = vec![0u8; len as usize];
    r.read_exact(&mut payload).await?;
    if header.ty == WINDOW_UPDATE
        && u32::from_be_bytes(payload[..4].try_into().unwrap()) & 0x7fff_ffff == 0
    {
        return Err(FetchError::Http("h2: zero window update".into()));
    }
    if header.ty == PRIORITY
        && u32::from_be_bytes(payload[..4].try_into().unwrap()) & 0x7fff_ffff == header.stream_id
    {
        return Err(FetchError::Http("h2: self-dependent priority".into()));
    }
    Ok((header, payload))
}

pub async fn write_frame<W: AsyncWriteExt + Unpin>(
    w: &mut W,
    ty: u8,
    flags: u8,
    stream_id: u32,
    payload: &[u8],
) -> Result<(), FetchError> {
    if payload.len() > 0xff_ffff || stream_id > 0x7fff_ffff {
        return Err(FetchError::Http(
            "h2: frame payload or stream id exceeds wire range".into(),
        ));
    }
    let len = payload.len() as u32;
    let hdr = [
        (len >> 16) as u8,
        (len >> 8) as u8,
        len as u8,
        ty,
        flags,
        ((stream_id >> 24) & 0x7f) as u8,
        (stream_id >> 16) as u8,
        (stream_id >> 8) as u8,
        stream_id as u8,
    ];
    w.write_all(&hdr).await?;
    w.write_all(payload).await?;
    Ok(())
}

pub fn settings_payload(pairs: &[(u16, u32)]) -> Vec<u8> {
    let mut out = Vec::with_capacity(pairs.len() * 6);
    for (id, val) in pairs {
        out.extend_from_slice(&id.to_be_bytes());
        out.extend_from_slice(&val.to_be_bytes());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn stealth_v3_h2_frame_lengths_and_stream_scope_are_enforced() {
        for (ty, flags, stream, payload) in [
            (PING, 0, 0, vec![0; 7]),
            (PING, 0, 1, vec![0; 8]),
            (RST_STREAM, 0, 0, vec![0; 4]),
            (SETTINGS, FLAG_ACK, 0, vec![0; 6]),
            (SETTINGS, 0, 0, vec![0; 5]),
            (SETTINGS, 0, 1, vec![]),
            (DATA, 0, 0, vec![]),
            (WINDOW_UPDATE, 0, 0, vec![0; 4]),
            (GOAWAY, 0, 0, vec![0; 7]),
            (0xfe, 0, 0, vec![0; 16_385]),
        ] {
            let mut bytes = Vec::new();
            write_frame(&mut bytes, ty, flags, stream, &payload)
                .await
                .unwrap();
            let result = read_frame(&mut bytes.as_slice()).await;
            assert!(
                result.is_err(),
                "invalid frame accepted: type={ty} flags={flags} stream={stream} len={}",
                payload.len()
            );
        }
        let mut bytes = Vec::new();
        // Unknown frame types/flags are extensible, not protocol errors.
        write_frame(&mut bytes, 0xfe, 0xff, 0, b"owned")
            .await
            .unwrap();
        assert_eq!(read_frame(&mut bytes.as_slice()).await.unwrap().1, b"owned");
    }

    #[tokio::test]
    async fn stealth_v3_h2_writer_refuses_truncating_a_reserved_stream_bit() {
        let mut bytes = Vec::new();
        assert!(
            write_frame(&mut bytes, HEADERS, FLAG_END_HEADERS, 0x8000_0001, &[])
                .await
                .is_err()
        );
        assert!(
            bytes.is_empty(),
            "invalid input must fail before a partial write"
        );
    }
}
