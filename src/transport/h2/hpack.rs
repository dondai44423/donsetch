//! HPACK (RFC 7541). Full decoder (incl. Huffman), Chrome-style encoder.

use std::collections::HashMap;
use std::sync::OnceLock;

use super::tables::{HUFFMAN, STATIC_TABLE};
use crate::error::FetchError;

const DYNAMIC_MAX: usize = 65536; // Chrome HEADER_TABLE_SIZE

// ---------- integer coding (§5.1) ----------

fn encode_int(out: &mut Vec<u8>, mut value: u64, prefix_bits: u8, flags: u8) {
    let max_prefix = (1u64 << prefix_bits) - 1;
    if value < max_prefix {
        out.push(flags | value as u8);
        return;
    }
    out.push(flags | max_prefix as u8);
    value -= max_prefix;
    while value >= 128 {
        out.push((value as u8 & 0x7f) | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

fn decode_int(buf: &[u8], pos: &mut usize, prefix_bits: u8) -> Result<u64, FetchError> {
    if *pos >= buf.len() {
        return Err(FetchError::Http("hpack: truncated int".into()));
    }
    let max_prefix = (1u64 << prefix_bits) - 1;
    let mut value = (buf[*pos] as u64) & max_prefix;
    *pos += 1;
    if value < max_prefix {
        return Ok(value);
    }
    let mut shift = 0u32;
    loop {
        if *pos >= buf.len() {
            return Err(FetchError::Http("hpack: truncated int continuation".into()));
        }
        let b = buf[*pos];
        *pos += 1;
        value = value
            .checked_add(((b & 0x7f) as u64) << shift)
            .ok_or_else(|| FetchError::Http("hpack: int overflow".into()))?;
        shift += 7;
        if b & 0x80 == 0 {
            return Ok(value);
        }
        if shift > 56 {
            return Err(FetchError::Http("hpack: int overflow".into()));
        }
    }
}

// ---------- huffman ----------

type HuffLookup = Vec<HashMap<u32, u16>>; // index = bit length

fn huff_lookup() -> &'static HuffLookup {
    static LOOKUP: OnceLock<HuffLookup> = OnceLock::new();
    LOOKUP.get_or_init(|| {
        let mut maps: HuffLookup = (0..=30).map(|_| HashMap::new()).collect();
        for (sym, &(code, bits)) in HUFFMAN.iter().enumerate() {
            if bits > 0 && bits <= 30 {
                maps[bits as usize].insert(code, sym as u16);
            }
        }
        maps
    })
}

pub fn huffman_decode(data: &[u8]) -> Result<Vec<u8>, FetchError> {
    let lookup = huff_lookup();
    let mut out = Vec::with_capacity(data.len() * 2);
    let mut code: u32 = 0;
    let mut len: u8 = 0;
    for &byte in data {
        for bit_idx in (0..8).rev() {
            let bit = (byte >> bit_idx) & 1;
            code = (code << 1) | bit as u32;
            len += 1;
            if len > 30 {
                return Err(FetchError::Http("hpack: huffman code too long".into()));
            }
            if let Some(&sym) = lookup[len as usize].get(&code) {
                if sym == 256 {
                    return Err(FetchError::Http("hpack: EOS in string".into()));
                }
                out.push(sym as u8);
                code = 0;
                len = 0;
            }
        }
    }
    // Remaining bits must be a prefix of EOS (all ones), at most 7 bits.
    if len >= 8 || (len > 0 && code != (1u32 << len) - 1) {
        return Err(FetchError::Http("hpack: bad huffman padding".into()));
    }
    Ok(out)
}

// ---------- strings ----------

/// Huffman-encode a byte string per RFC 7541 Appendix B.
/// Returns the encoded bytes (may be longer than input for short strings).
fn huffman_encode(input: &[u8]) -> Vec<u8> {
    let mut bit_buf: u64 = 0;
    let mut bit_count: u32 = 0;
    let mut out = Vec::with_capacity(input.len());
    for &byte in input {
        let (code, bits) = HUFFMAN[byte as usize];
        bit_buf = (bit_buf << bits) | code as u64;
        bit_count += bits as u32;
        while bit_count >= 8 {
            bit_count -= 8;
            out.push((bit_buf >> bit_count) as u8);
        }
    }
    // Pad remaining bits with EOS prefix (all 1-bits) to byte boundary.
    if bit_count > 0 {
        let pad = 8 - bit_count;
        out.push(((bit_buf << pad) | ((1u64 << pad) - 1)) as u8);
    }
    out
}

fn encode_string(out: &mut Vec<u8>, s: &[u8]) {
    // Chrome Huffman-encodes when shorter; raw otherwise.
    let huff = huffman_encode(s);
    if huff.len() < s.len() {
        encode_int(out, huff.len() as u64, 7, 0x80); // H=1
        out.extend_from_slice(&huff);
    } else {
        encode_int(out, s.len() as u64, 7, 0); // H=0
        out.extend_from_slice(s);
    }
}

fn decode_string(buf: &[u8], pos: &mut usize) -> Result<Vec<u8>, FetchError> {
    if *pos >= buf.len() {
        return Err(FetchError::Http("hpack: truncated string".into()));
    }
    let huff = buf[*pos] & 0x80 != 0;
    let len = usize::try_from(decode_int(buf, pos, 7)?)
        .map_err(|_| FetchError::Http("hpack: string size overflow".into()))?;
    let end = pos
        .checked_add(len)
        .filter(|end| *end <= buf.len())
        .ok_or_else(|| FetchError::Http("hpack: truncated string data".into()))?;
    let raw = &buf[*pos..end];
    *pos = end;
    if huff {
        huffman_decode(raw)
    } else {
        Ok(raw.to_vec())
    }
}

// ---------- dynamic table ----------

struct DynTable {
    entries: Vec<(Vec<u8>, Vec<u8>)>, // newest at end
    size: usize,
    max: usize,
}

impl DynTable {
    fn new(max: usize) -> Self {
        Self {
            entries: Vec::new(),
            size: 0,
            max,
        }
    }
    fn insert(&mut self, name: Vec<u8>, value: Vec<u8>) {
        let esz = name.len() + value.len() + 32;
        self.entries.push((name, value));
        self.size += esz;
        while self.size > self.max && !self.entries.is_empty() {
            let (n, v) = self.entries.remove(0);
            self.size -= n.len() + v.len() + 32;
        }
    }
    fn set_max(&mut self, max: usize) {
        self.max = max;
        while self.size > max && !self.entries.is_empty() {
            let (name, value) = self.entries.remove(0);
            self.size -= name.len() + value.len() + 32;
        }
    }

    fn exact(&self, name: &str, value: &str) -> Option<usize> {
        STATIC_TABLE
            .iter()
            .position(|(n, v)| *n == name && *v == value)
            .map(|i| i + 1)
            .or_else(|| {
                self.entries
                    .iter()
                    .rev()
                    .position(|(n, v)| n == name.as_bytes() && v == value.as_bytes())
                    .map(|i| STATIC_TABLE.len() + 1 + i)
            })
    }

    fn name(&self, name: &str) -> Option<usize> {
        STATIC_TABLE
            .iter()
            .position(|(n, _)| *n == name)
            .map(|i| i + 1)
            .or_else(|| {
                self.entries
                    .iter()
                    .rev()
                    .position(|(n, _)| n == name.as_bytes())
                    .map(|i| STATIC_TABLE.len() + 1 + i)
            })
    }
    /// Absolute index: 1..=61 static, 62.. dynamic (newest first).
    fn get(&self, idx: usize) -> Option<(Vec<u8>, Vec<u8>)> {
        if idx >= 1 && idx <= STATIC_TABLE.len() {
            let (n, v) = STATIC_TABLE[idx - 1];
            return Some((n.as_bytes().to_vec(), v.as_bytes().to_vec()));
        }
        let Some(dyn_idx) = idx.checked_sub(STATIC_TABLE.len() + 1) else {
            return None; // hostile index 0 / protocol violation
        }; // 0 = newest
        if dyn_idx < self.entries.len() {
            return Some(self.entries[self.entries.len() - 1 - dyn_idx].clone());
        }
        None
    }
}

// ---------- encoder ----------

pub struct Encoder {
    dyn_table: DynTable,
    pending_max: Option<(usize, usize)>,
}

impl Default for Encoder {
    fn default() -> Self {
        Self::new()
    }
}

impl Encoder {
    pub fn new() -> Self {
        Self {
            // Our advertised table size governs the response decoder.
            // Request encoding starts at the peer's RFC default until SETTINGS.
            dyn_table: DynTable::new(4096),
            pending_max: None,
        }
    }

    pub fn set_max(&mut self, peer_max: u32) {
        let max = (peer_max as usize).min(DYNAMIC_MAX);
        if max != self.dyn_table.max {
            let min = self.pending_max.map_or(max, |(min, _)| min.min(max));
            self.pending_max = Some((min, max));
            self.dyn_table.set_max(max);
        }
    }

    /// Encode a header list in order. Indexed for exact retained matches,
    /// literal-with-incremental-indexing otherwise (Chrome's strategy).
    /// Sensitive headers (cookie, authorization) use never-indexed to
    /// match Chrome's HPACK encoder : keeps the dynamic table identical.
    pub fn encode(&mut self, headers: &[(String, String)]) -> Vec<u8> {
        let mut out = Vec::new();
        if let Some((min, max)) = self.pending_max.take() {
            encode_int(&mut out, min as u64, 5, 0x20);
            if max != min {
                encode_int(&mut out, max as u64, 5, 0x20);
            }
        }
        for (name, value) in headers {
            let name_l = name.to_ascii_lowercase();
            // Chrome marks sensitive headers as never-indexed to prevent
            // them from entering the dynamic table.
            let sensitive = matches!(
                name_l.as_str(),
                "cookie" | "authorization" | "proxy-authorization"
            );
            if !sensitive && let Some(index) = self.dyn_table.exact(&name_l, value) {
                encode_int(&mut out, index as u64, 7, 0x80);
                continue;
            }
            let name_idx = self.dyn_table.name(&name_l);
            if sensitive {
                // Never indexed (0x10 prefix, 4-bit integer).
                match name_idx {
                    Some(i) => encode_int(&mut out, i as u64, 4, 0x10),
                    None => {
                        out.push(0x10);
                        encode_string(&mut out, name_l.as_bytes());
                    }
                }
                encode_string(&mut out, value.as_bytes());
                // Do NOT insert into dynamic table.
            } else {
                // Literal with incremental indexing (0x40 prefix, 6-bit integer).
                match name_idx {
                    Some(i) => encode_int(&mut out, i as u64, 6, 0x40),
                    None => {
                        out.push(0x40);
                        encode_string(&mut out, name_l.as_bytes());
                    }
                }
                encode_string(&mut out, value.as_bytes());
                self.dyn_table
                    .insert(name_l.into_bytes(), value.clone().into_bytes());
            }
        }
        out
    }
}

// ---------- decoder ----------

pub struct Decoder {
    dyn_table: DynTable,
    ceiling: usize,
}

impl Default for Decoder {
    fn default() -> Self {
        Self::new()
    }
}

impl Decoder {
    pub fn new() -> Self {
        // RFC 7541 §4.2/§6.3: the decoder's INITIAL maximum table size is
        // the SETTINGS_HEADER_TABLE_SIZE we advertised (Chrome's 65536,
        // the same DYNAMIC_MAX the size-update check enforces). Starting at
        // 4096 made us evict entries a peer that keeps the default and
        // never sends a size update is entitled to index: the absolute
        // index then resolved past the end of our table and the whole
        // response died with "hpack: bad index N". A larger decoder table
        // than the peer uses is free; a smaller one breaks the wire.
        Self::with_limit(DYNAMIC_MAX)
    }

    pub fn with_limit(ceiling: usize) -> Self {
        let ceiling = ceiling.min(DYNAMIC_MAX);
        Self {
            dyn_table: DynTable::new(ceiling),
            ceiling,
        }
    }

    pub fn decode(&mut self, block: &[u8]) -> Result<Vec<(String, String)>, FetchError> {
        let mut headers = Vec::new();
        let mut pos = 0usize;
        let mut decoded_size = 0usize;
        while pos < block.len() {
            let b = block[pos];
            if b & 0x80 != 0 {
                // Indexed.
                let idx = usize::try_from(decode_int(block, &mut pos, 7)?)
                    .map_err(|_| FetchError::Http("hpack: index overflow".into()))?;
                let (n, v) = self
                    .dyn_table
                    .get(idx)
                    .ok_or_else(|| FetchError::Http(format!("hpack: bad index {idx}")))?;
                headers.push((
                    String::from_utf8_lossy(&n).into(),
                    String::from_utf8_lossy(&v).into(),
                ));
            } else if b & 0xc0 == 0x40 {
                // Literal, incremental indexing.
                let (name, value) = self.decode_literal(block, &mut pos, 6)?;
                self.dyn_table
                    .insert(name.clone().into_bytes(), value.clone().into_bytes());
                headers.push((name, value));
            } else if b & 0xe0 == 0x20 {
                // Dynamic table size update.
                let new_max = usize::try_from(decode_int(block, &mut pos, 5)?)
                    .map_err(|_| FetchError::Http("hpack: table size overflow".into()))?;
                // RFC 7541 §4.2: must not exceed what we advertised
                // (Chrome's HEADER_TABLE_SIZE = 65536). An uncapped
                // update lets a hostile server balloon our decoder
                // table without bound.
                if !headers.is_empty() {
                    return Err(FetchError::Http(
                        "hpack: table size update after fields".into(),
                    ));
                }
                if new_max > self.ceiling {
                    return Err(FetchError::Http(format!(
                        "hpack: table size update {new_max} exceeds {}",
                        self.ceiling
                    )));
                }
                self.dyn_table.set_max(new_max);
            } else {
                // Literal without indexing (0x00) / never indexed (0x10).
                let (name, value) = self.decode_literal(block, &mut pos, 4)?;
                headers.push((name, value));
            }
            if let Some((name, value)) = headers.last() {
                decoded_size += name.len() + value.len() + 32;
                if decoded_size > 256 << 10 {
                    return Err(FetchError::Http("hpack: decoded fields exceed cap".into()));
                }
            }
        }
        Ok(headers)
    }

    fn decode_literal(
        &mut self,
        block: &[u8],
        pos: &mut usize,
        prefix: u8,
    ) -> Result<(String, String), FetchError> {
        let name_idx = usize::try_from(decode_int(block, pos, prefix)?)
            .map_err(|_| FetchError::Http("hpack: name index overflow".into()))?;
        let name = if name_idx == 0 {
            decode_string(block, pos)?
        } else {
            self.dyn_table
                .get(name_idx)
                .ok_or_else(|| FetchError::Http(format!("hpack: bad name index {name_idx}")))?
                .0
        };
        let value = decode_string(block, pos)?;
        Ok((
            String::from_utf8_lossy(&name).into(),
            String::from_utf8_lossy(&value).into(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stealth_v3_hpack_repeated_fields_use_dynamic_indices() {
        let fields = vec![("x-owned-proof".into(), "a repeated value".into())];
        let mut encoder = Encoder::new();
        let first = encoder.encode(&fields);
        let second = encoder.encode(&fields);
        assert!(
            second.len() < first.len(),
            "repeated fields should use the retained table"
        );
        let mut decoder = Decoder::new();
        assert_eq!(decoder.decode(&first).unwrap(), fields);
        assert_eq!(decoder.decode(&second).unwrap(), fields);
    }

    #[test]
    fn stealth_v3_hpack_secrets_are_never_indexed_even_when_empty() {
        let mut encoder = Encoder::new();
        let mut decoder = Decoder::new();
        for name in ["cookie", "authorization", "proxy-authorization"] {
            for value in ["", "secret"] {
                let encoded = encoder.encode(&[(name.into(), value.into())]);
                assert_eq!(
                    encoded[0] & 0xf0,
                    0x10,
                    "{name} must be never-indexed even when empty"
                );
                assert_eq!(
                    decoder.decode(&encoded).unwrap(),
                    vec![(name.into(), value.into())]
                );
            }
        }
    }

    #[test]
    fn stealth_v3_hpack_encoder_starts_at_the_peers_default_4096() {
        assert_eq!(Encoder::new().dyn_table.max, 4096);
    }

    #[test]
    fn stealth_v3_hpack_shrink_then_grow_synchronizes_the_peers_table() {
        let fields = vec![("x-owned".into(), "proof".into())];
        let mut encoder = Encoder::new();
        let mut decoder = Decoder::new();
        assert_eq!(decoder.decode(&encoder.encode(&fields)).unwrap(), fields);
        encoder.set_max(0);
        encoder.set_max(64);
        let block = encoder.encode(&fields);
        assert_eq!(
            &block[..3],
            &[0x20, 0x3f, 0x21],
            "announce the minimum before the final size"
        );
        assert_eq!(decoder.decode(&block).unwrap(), fields);
        assert_eq!(decoder.dyn_table.max, 64);
        assert_eq!(decoder.decode(&encoder.encode(&fields)).unwrap(), fields);
    }

    #[test]
    fn stealth_v3_hpack_rejects_late_updates_and_decoded_index_amplification() {
        assert!(
            Decoder::new()
                .decode(&[0x88, 0x20])
                .unwrap_err()
                .to_string()
                .contains("after fields")
        );
        let mut decoder = Decoder::new();
        decoder
            .decode(&literal_with_indexing("x-owned", 1000))
            .unwrap();
        let error = decoder.decode(&[0xbe; 300]).unwrap_err();
        assert!(error.to_string().contains("decoded fields exceed cap"));
        let mut malformed = vec![0x00, 0x7f];
        malformed.extend_from_slice(&[0xff; 8]);
        malformed.push(0x7f);
        assert!(
            Decoder::new().decode(&malformed).is_err(),
            "hostile encoded lengths must return an error rather than overflow"
        );
    }

    /// A literal header field with incremental indexing and a brand-new
    /// name (0x40 + name index 0), value forced raw (H=0) so the byte
    /// count is exact.
    fn literal_with_indexing(name: &str, value_len: usize) -> Vec<u8> {
        let mut out = vec![0x40];
        encode_int(&mut out, name.len() as u64, 7, 0);
        out.extend_from_slice(name.as_bytes());
        encode_int(&mut out, value_len as u64, 7, 0);
        out.extend(std::iter::repeat_n(b'x', value_len));
        out
    }

    /// The decoder's table must be as large as the HEADER_TABLE_SIZE we
    /// advertise (65536): a peer that keeps the default and never sends a
    /// dynamic table size update may index anything it inserted, and an
    /// evicted entry is a "bad index" that fails the response. Old code
    /// started the decoder at 4096 and dropped the entries below.
    #[test]
    fn decoder_table_starts_at_the_advertised_size() {
        assert_eq!(Decoder::new().dyn_table.max, DYNAMIC_MAX);

        let mut d = Decoder::new();
        // Five ~1 KiB entries: ~5.2 KiB of table, past the old 4096 initial
        // max but well under the 65536 we advertise.
        let mut block = Vec::new();
        for i in 0..5 {
            block.extend(literal_with_indexing(&format!("x-fill-{i}"), 1000));
        }
        // Absolute index of the OLDEST of the five: 61 static entries, then
        // the dynamic table newest-first (62 = newest, 66 = oldest).
        encode_int(&mut block, 66, 7, 0x80);
        let decoded = d.decode(&block).expect("the oldest entry must still index");
        assert_eq!(decoded.len(), 6);
        assert_eq!(decoded[5].0, "x-fill-0");
    }

    /// The size-update cap stays: a peer may not grow our decoder table
    /// above what we advertised, whatever it sends.
    #[test]
    fn size_update_above_the_advertised_ceiling_is_refused() {
        let mut d = Decoder::new();
        let mut block = Vec::new();
        encode_int(&mut block, (DYNAMIC_MAX + 1) as u64, 5, 0x20);
        assert!(d.decode(&block).is_err());
    }
}
