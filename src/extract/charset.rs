//! Charset detection + decoding. Never trust "UTF-8 always".
//!
//! Detection order (matches browser behavior):
//! 1. Content-Type header charset (case-insensitive)
//! 2. BOM
//! 3. HTML5 charset prescan (first 4 KB, handles all meta forms)
//! 4. Statistical: valid-UTF-8 check, then CJK byte-pattern heuristic
//! 5. UTF-8 lossy (last resort, produces replacement chars)
//!
//! CJK pages are the pain point: many older Chinese sites use
//! GBK/GB2312/Big5 without declaring a charset, and a bare UTF-8
//! lossy fallback turns every CJK byte pair into U+FFFD tofu.

/// Content-Type to pass for browser-provided (ghost) DOM text.
///
/// The ghost tier reads text out of a live Chromium DOM via CDP,
/// which returns UTF-8 strings : the browser already ran charset
/// decoding. But the rendered DOM still carries the page's original
/// `<meta charset=...>` declaration (e.g. gb18030 on 69shuba), so a
/// bare `text/html` makes `decode` sniff that stale meta and decode
/// the already-UTF-8 bytes a second time as gb18030 → mojibake (#35).
/// Declaring `charset=utf-8` here wins at step 1 (header beats meta)
/// and copies the text through verbatim. Raw HTTP bytes keep full
/// detection : only browser text is pinned.
pub const GHOST_TEXT_CT: &str = "text/html; charset=utf-8";

/// Decode body bytes to a String using the detection order above.
pub fn decode(body: &[u8], content_type: &str) -> String {
    if let Some(enc) = from_content_type(content_type) {
        return enc.decode(body).0.into_owned();
    }
    if let Some(enc) = from_bom(body) {
        return enc.decode(body).0.into_owned();
    }
    if let Some(enc) = sniff_meta(body) {
        return enc.decode(body).0.into_owned();
    }
    if let Some(enc) = statistical_detect(body) {
        return enc.decode(body).0.into_owned();
    }
    String::from_utf8_lossy(body).into_owned()
}

/// Decode a body that is not HTML (plain text, JSON, XML, a feed) by
/// what it declares, without guessing: a BOM, else the Content-Type
/// charset, else the `encoding` of a leading `<?xml ...?>` declaration,
/// else UTF-8. A Content-Type charset that names the replacement
/// encoding (iso-2022-kr, hz-gb-2312 and the other labels encoding_rs
/// maps to it) is ignored. Malformed bytes become U+FFFD.
pub fn decode_declared(body: &[u8], content_type: &str) -> String {
    // No meta prescan and no statistical guess: the prescan takes the
    // label after any "charset" word in the first 4 KB, so a UTF-8 text
    // that mentions one would decode as that charset. `Encoding::decode`
    // sniffs a BOM ahead of the label it is called on, which puts the BOM
    // first (RFC 7303 §3.2 for XML: the BOM, then the charset parameter).
    // The replacement decoder turns any non-empty input into one U+FFFD,
    // so following such a header would drop the whole body.
    let enc = from_content_type(content_type)
        .filter(|enc| *enc != encoding_rs::REPLACEMENT)
        .or_else(|| xml_declared(body))
        .unwrap_or(encoding_rs::UTF_8);
    enc.decode(body).0.into_owned()
}

/// The encoding an XML declaration at the start of `body` names
/// (leading ASCII whitespace allowed). A UTF-16 or replacement label
/// yields UTF-8.
fn xml_declared(body: &[u8]) -> Option<&'static encoding_rs::Encoding> {
    let head = &body[..body.len().min(1024)];
    let decl = head.trim_ascii_start().strip_prefix(b"<?xml")?;
    let decl = &decl[..decl.windows(2).position(|w| w == b"?>")?];
    let at = decl.windows(8).position(|w| w == b"encoding")?;
    let value = decl[at + 8..]
        .trim_ascii_start()
        .strip_prefix(b"=")?
        .trim_ascii_start();
    let (&quote, value) = value.split_first()?;
    if quote != b'"' && quote != b'\'' {
        return None;
    }
    let label = &value[..value.iter().position(|&b| b == quote)?];
    // A declaration read as ASCII bytes is not in UTF-16, whatever it
    // names, and the replacement encoding would turn the whole body into
    // one U+FFFD: both decode as UTF-8.
    encoding_rs::Encoding::for_label(label).map(encoding_rs::Encoding::output_encoding)
}

/// Extract charset from a Content-Type header value.
///
/// Handles: case-insensitive "charset", quoted values, spaces
/// around `=`, and values terminated by `;`, space, quote, or EOL.
fn from_content_type(ct: &str) -> Option<&'static encoding_rs::Encoding> {
    let lower = ct.to_ascii_lowercase();
    let idx = lower.find("charset")?;
    let rest = &ct[idx + 7..];
    let rest = rest.trim_start();
    let rest = rest.strip_prefix('=')?;
    let rest = rest.trim_start();
    let rest = rest.trim_start_matches(['"', '\'']);
    let label: String = rest
        .chars()
        .take_while(|c| !matches!(c, ';' | '"' | '\'' | ' '))
        .collect();
    if label.is_empty() {
        return None;
    }
    encoding_rs::Encoding::for_label(label.as_bytes())
}

fn from_bom(body: &[u8]) -> Option<&'static encoding_rs::Encoding> {
    if body.starts_with(&[0xEF, 0xBB, 0xBF]) {
        Some(encoding_rs::UTF_8)
    } else if body.starts_with(&[0xFF, 0xFE]) {
        Some(encoding_rs::UTF_16LE)
    } else if body.starts_with(&[0xFE, 0xFF]) {
        Some(encoding_rs::UTF_16BE)
    } else {
        None
    }
}

/// HTML5 charset prescan. Searches the first 4 KB for charset
/// declarations in both `<meta charset>` and
/// `<meta http-equiv=content-type content="...; charset=...">`
/// forms. Works on raw bytes (ASCII-safe) to avoid corrupting
/// non-UTF-8 content before the charset is known.
fn sniff_meta(body: &[u8]) -> Option<&'static encoding_rs::Encoding> {
    let head = &body[..body.len().min(4096)];
    let text = String::from_utf8_lossy(head);
    let lower = text.to_ascii_lowercase();

    let mut search_from = 0;
    while let Some(idx) = lower[search_from..].find("charset") {
        let abs = search_from + idx;
        let lower_rest = &lower[abs + 7..];

        let trimmed = lower_rest.trim_start();
        let after_eq = trimmed.strip_prefix('=').unwrap_or(trimmed);
        let after_eq = after_eq.trim_start();
        let after_eq = after_eq.trim_start_matches(['"', '\'']);

        let label: String = after_eq
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
            .collect();

        if !label.is_empty()
            && let Some(enc) = encoding_rs::Encoding::for_label(label.as_bytes())
        {
            return Some(enc);
        }

        search_from = abs + 7;
    }

    None
}

/// Statistical encoding detection for pages with no declared
/// charset. Returns the most likely encoding, or None if UTF-8
/// lossy is the best we can do.
///
/// Strategy:
/// 1. Valid UTF-8 (or mostly valid) -> UTF-8.
/// 2. Not valid UTF-8 -> CJK byte-pattern analysis. GBK is a
///    superset of Big5 and EUC-KR in valid byte patterns, so
///    we look for encoding-specific markers:
///    - GBK-only: lead byte 0x81-0xA0 or trail byte 0x80-0xA0
///    - Big5-only: trail byte 0x40-0x7E (EUC-KR requires >= 0xA1)
///    - EUC-KR: all lead+trail bytes in 0xA1-0xFE
/// 3. No CJK pattern -> None (caller falls back to UTF-8 lossy).
fn statistical_detect(body: &[u8]) -> Option<&'static encoding_rs::Encoding> {
    if body.is_empty() {
        return None;
    }

    // 1. Valid UTF-8? Fast path.
    if std::str::from_utf8(body).is_ok() {
        return Some(encoding_rs::UTF_8);
    }

    // 2. Mostly-valid UTF-8: < 1% bad bytes -> UTF-8 with corruption.
    let lossy = String::from_utf8_lossy(body);
    let repl_count = lossy.matches('\u{FFFD}').count();
    if body.len() > 100 && repl_count < body.len() / 100 {
        return Some(encoding_rs::UTF_8);
    }

    // 3. CJK byte-pattern analysis.
    let mut gbk_only = 0usize;
    let mut total_pairs = 0usize;
    let mut low_trail = 0usize;
    let mut all_trail_high = true;
    let mut i = 0;
    while i < body.len() {
        let b = body[i];
        if b < 0x80 {
            i += 1;
            continue;
        }
        // Check if the next byte could be a CJK trail byte.
        // GBK trail: 0x40-0x7E or 0x80-0xFE (not 0x7F)
        // Big5 trail: 0x40-0x7E or 0xA1-0xFE
        // EUC-KR trail: 0xA1-0xFE
        let is_trail = i + 1 < body.len()
            && (((0x40..=0x7E).contains(&body[i + 1]) || (0x80..=0xFE).contains(&body[i + 1]))
                && body[i + 1] != 0x7F);
        if is_trail {
            let lead = b;
            let trail = body[i + 1];
            total_pairs += 1;

            // GBK-only: lead byte 0x81-0xA0 (Big5/EUC-KR start at 0xA1)
            if (0x81..=0xA0).contains(&lead) {
                gbk_only += 1;
            }
            // GBK-only: trail byte 0x80-0xA0 (Big5/EUC-KR skip)
            if (0x80..=0xA0).contains(&trail) {
                gbk_only += 1;
                all_trail_high = false;
            }
            // Big5-only: trail byte 0x40-0x7E (EUC-KR requires >= 0xA1)
            if (0x40..=0x7E).contains(&trail) {
                low_trail += 1;
                all_trail_high = false;
            }
            i += 2;
        } else {
            // Lone high byte: only GBK accepts these.
            gbk_only += 1;
            i += 1;
        }
    }

    if total_pairs == 0 {
        return None;
    }

    let pair_ratio = total_pairs as f64 / body.len() as f64;
    if pair_ratio < 0.05 {
        return None;
    }

    if gbk_only > 0 {
        // Could be GBK (Chinese) or Shift-JIS (Japanese), since both
        // use lead bytes 0x81-0x9F. Decode a sample with Shift-JIS
        // and check for kana: Japanese text produces hiragana/katakana,
        // Chinese text produces only Han.
        let sample = &body[..body.len().min(4096)];
        let sjis_text = encoding_rs::SHIFT_JIS.decode(sample).0.into_owned();
        let kana = sjis_text
            .chars()
            .filter(|c| {
                let u = *c as u32;
                (0x3040..=0x30FF).contains(&u) // Hiragana + Katakana
                    || (0xFF66..=0xFF9F).contains(&u) // half-width katakana
            })
            .count();
        if kana > 5 {
            Some(encoding_rs::SHIFT_JIS)
        } else {
            Some(encoding_rs::GBK)
        }
    } else if low_trail > 0 {
        Some(encoding_rs::BIG5)
    } else if all_trail_high {
        // Ambiguous: all lead+trail bytes in 0xA1-0xFE. Could be
        // GBK, Big5, EUC-KR, or EUC-JP. Decode samples with each and
        // check for script-specific characters:
        //   Korean -> Hangul (EUC-KR decode)
        //   Japanese -> kana (EUC-JP decode)
        //   Chinese -> Han only (GBK decode)
        let sample = &body[..body.len().min(4096)];
        let gbk_text = encoding_rs::GBK.decode(sample).0.into_owned();
        let euckr_text = encoding_rs::EUC_KR.decode(sample).0.into_owned();
        let eucjp_text = encoding_rs::EUC_JP.decode(sample).0.into_owned();
        let gbk_han = gbk_text
            .chars()
            .filter(|c| ('\u{4E00}'..='\u{9FFF}').contains(c))
            .count();
        let euckr_hangul = euckr_text
            .chars()
            .filter(|c| ('\u{AC00}'..='\u{D7AF}').contains(c))
            .count();
        let eucjp_kana = eucjp_text
            .chars()
            .filter(|c| {
                let u = *c as u32;
                (0x3040..=0x30FF).contains(&u) || (0xFF66..=0xFF9F).contains(&u)
            })
            .count();
        if euckr_hangul > 0 && euckr_hangul >= gbk_han {
            Some(encoding_rs::EUC_KR)
        } else if eucjp_kana > 5 {
            Some(encoding_rs::EUC_JP)
        } else {
            Some(encoding_rs::GBK)
        }
    } else {
        Some(encoding_rs::GBK)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CHINESE_TEXT: &str = "机器学习是人工智能的一个分支领域。深度学习使用神经网络。";
    const TRADITIONAL_TEXT: &str = "機器學習是人工智能的一個分支領域。深度學習使用神經網路。";

    fn gbk_page(text: &str, meta: Option<&str>) -> Vec<u8> {
        let (body, _, _) = encoding_rs::GBK.encode(text);
        let head = match meta {
            Some(m) => format!("<html><head>{m}</head><body>"),
            None => "<html><head><title>Test</title></head><body>".to_string(),
        };
        let mut page = Vec::new();
        page.extend_from_slice(head.as_bytes());
        page.extend_from_slice(&body);
        page.extend_from_slice(b"</body></html>");
        page
    }

    fn big5_page(text: &str, meta: Option<&str>) -> Vec<u8> {
        let (body, _, had_unmappable) = encoding_rs::BIG5.encode(text);
        assert!(
            !had_unmappable,
            "Big5 test text has unmappable chars: {text}"
        );
        let head = match meta {
            Some(m) => format!("<html><head>{m}</head><body>"),
            None => "<html><head><title>Test</title></head><body>".to_string(),
        };
        let mut page = Vec::new();
        page.extend_from_slice(head.as_bytes());
        page.extend_from_slice(&body);
        page.extend_from_slice(b"</body></html>");
        page
    }

    // ── Content-Type header parsing ──

    #[test]
    fn from_ct_lowercase() {
        assert_eq!(
            from_content_type("text/html; charset=utf-8"),
            Some(encoding_rs::UTF_8)
        );
    }

    #[test]
    fn from_ct_capital_charset() {
        assert_eq!(
            from_content_type("text/html; Charset=utf-8"),
            Some(encoding_rs::UTF_8)
        );
    }

    #[test]
    fn from_ct_all_caps_charset() {
        assert_eq!(
            from_content_type("text/html; CHARSET=UTF-8"),
            Some(encoding_rs::UTF_8)
        );
    }

    #[test]
    fn from_ct_double_quoted() {
        assert_eq!(
            from_content_type("text/html; charset=\"utf-8\""),
            Some(encoding_rs::UTF_8)
        );
    }

    #[test]
    fn from_ct_single_quoted() {
        assert_eq!(
            from_content_type("text/html; charset='utf-8'"),
            Some(encoding_rs::UTF_8)
        );
    }

    #[test]
    fn from_ct_spaces_around_equals() {
        assert_eq!(
            from_content_type("text/html; charset = utf-8"),
            Some(encoding_rs::UTF_8)
        );
    }

    #[test]
    fn from_ct_gbk() {
        assert_eq!(
            from_content_type("text/html; charset=gbk"),
            Some(encoding_rs::GBK)
        );
    }

    #[test]
    fn from_ct_gb2312() {
        assert_eq!(
            from_content_type("text/html; charset=gb2312"),
            Some(encoding_rs::GBK)
        );
    }

    #[test]
    fn from_ct_big5() {
        assert_eq!(
            from_content_type("text/html; charset=big5"),
            Some(encoding_rs::BIG5)
        );
    }

    #[test]
    fn from_ct_no_charset() {
        assert_eq!(from_content_type("text/html"), None);
    }

    #[test]
    fn from_ct_empty() {
        assert_eq!(from_content_type(""), None);
    }

    // ── Meta charset sniffing ──

    #[test]
    fn sniff_meta_utf8() {
        let html = b"<html><head><meta charset=\"utf-8\"></head><body>hi</body></html>";
        assert_eq!(sniff_meta(html), Some(encoding_rs::UTF_8));
    }

    #[test]
    fn sniff_meta_gbk() {
        let html = b"<html><head><meta charset=\"gbk\"></head><body>hi</body></html>";
        assert_eq!(sniff_meta(html), Some(encoding_rs::GBK));
    }

    #[test]
    fn sniff_meta_big5() {
        let html = b"<html><head><meta charset=\"big5\"></head><body>hi</body></html>";
        assert_eq!(sniff_meta(html), Some(encoding_rs::BIG5));
    }

    #[test]
    fn sniff_meta_http_equiv() {
        let html = b"<html><head><meta http-equiv=\"Content-Type\" content=\"text/html; charset=gbk\"></head><body>hi</body></html>";
        assert_eq!(sniff_meta(html), Some(encoding_rs::GBK));
    }

    #[test]
    fn sniff_meta_no_charset() {
        let html = b"<html><head><title>No charset here</title></head><body>hi</body></html>";
        assert_eq!(sniff_meta(html), None);
    }

    #[test]
    fn sniff_meta_after_2kb() {
        let mut html = String::from("<html><head>");
        html.push_str(&"<span>x</span>".repeat(200));
        html.push_str("<meta charset=\"gbk\"></head><body>hi</body></html>");
        assert_eq!(sniff_meta(html.as_bytes()), Some(encoding_rs::GBK));
    }

    #[test]
    fn sniff_meta_case_insensitive() {
        let html = b"<html><head><META CHARSET=\"GBK\"></head><body>hi</body></html>";
        assert_eq!(sniff_meta(html), Some(encoding_rs::GBK));
    }

    // ── Statistical detection ──

    #[test]
    fn stat_detect_valid_utf8() {
        let html = "<html><body>机器学习</body></html>".as_bytes();
        assert_eq!(statistical_detect(html), Some(encoding_rs::UTF_8));
    }

    #[test]
    fn stat_detect_gbk_no_meta() {
        let page = gbk_page(CHINESE_TEXT, None);
        assert_eq!(statistical_detect(&page), Some(encoding_rs::GBK));
    }

    #[test]
    fn stat_detect_big5_no_meta() {
        let page = big5_page(TRADITIONAL_TEXT, None);
        assert_eq!(statistical_detect(&page), Some(encoding_rs::BIG5));
    }

    #[test]
    fn stat_detect_pure_ascii() {
        let html = b"<html><body>Hello World</body></html>";
        assert_eq!(statistical_detect(html), Some(encoding_rs::UTF_8));
    }

    #[test]
    fn stat_detect_empty() {
        assert_eq!(statistical_detect(&[]), None);
    }

    // ── Full decode pipeline ──

    #[test]
    fn decode_utf8_with_ct() {
        let body = "<html><body>机器学习</body></html>".as_bytes();
        let decoded = decode(body, "text/html; charset=utf-8");
        assert!(decoded.contains("机器学习"));
    }

    #[test]
    fn decode_gbk_with_ct() {
        let page = gbk_page(CHINESE_TEXT, None);
        let decoded = decode(&page, "text/html; charset=gbk");
        assert!(decoded.contains(CHINESE_TEXT));
    }

    #[test]
    fn decode_gbk_with_meta() {
        let page = gbk_page(CHINESE_TEXT, Some("<meta charset=\"gbk\">"));
        let decoded = decode(&page, "text/html");
        assert!(decoded.contains(CHINESE_TEXT));
    }

    #[test]
    fn decode_gbk_no_charset() {
        let page = gbk_page(CHINESE_TEXT, None);
        let decoded = decode(&page, "text/html");
        assert!(
            decoded.contains(CHINESE_TEXT),
            "expected Chinese text, got garbled output"
        );
        assert!(
            !decoded.contains('\u{FFFD}'),
            "no replacement characters should be present"
        );
    }

    #[test]
    fn decode_big5_no_charset() {
        let page = big5_page(TRADITIONAL_TEXT, None);
        let decoded = decode(&page, "text/html");
        assert!(
            decoded.contains(TRADITIONAL_TEXT),
            "expected Traditional Chinese text, got: {decoded}"
        );
        assert!(
            !decoded.contains('\u{FFFD}'),
            "no replacement characters should be present"
        );
    }

    #[test]
    fn decode_utf8_capital_charset() {
        let body = "<html><body>机器学习</body></html>".as_bytes();
        let decoded = decode(body, "text/html; Charset=UTF-8");
        assert!(decoded.contains("机器学习"));
    }

    #[test]
    fn decode_utf8_quoted_charset() {
        let body = "<html><body>机器学习</body></html>".as_bytes();
        let decoded = decode(body, "text/html; charset=\"utf-8\"");
        assert!(decoded.contains("机器学习"));
    }

    #[test]
    fn decode_utf8_spaces_around_equals() {
        let body = "<html><body>机器学习</body></html>".as_bytes();
        let decoded = decode(body, "text/html; charset = utf-8");
        assert!(decoded.contains("机器学习"));
    }

    #[test]
    fn decode_utf8_valid_no_ct() {
        let body = "<html><body>机器学习是人工智能</body></html>".as_bytes();
        let decoded = decode(body, "");
        assert!(decoded.contains("机器学习"));
    }

    #[test]
    fn decode_mixed_no_tofu_for_gbk() {
        let (gbk_bytes, _, _) = encoding_rs::GBK.encode("你好世界，这是一个测试。机器学习很有趣。");
        let html = {
            let mut p = Vec::new();
            p.extend_from_slice(b"<html><head><title>Test Page</title></head><body><p>");
            p.extend_from_slice(&gbk_bytes);
            p.extend_from_slice(b"</p></body></html>");
            p
        };
        let decoded = decode(&html, "text/html");
        assert!(decoded.contains("你好世界"), "decoded: {decoded}");
        assert!(decoded.contains("机器学习"), "decoded: {decoded}");
        assert!(!decoded.contains('\u{FFFD}'), "no tofu: {decoded}");
    }

    #[test]
    fn decode_korean_euckr_no_charset() {
        let korean = "안녕하세요. 이것은 한국어 텍스트입니다.";
        let (euckr_bytes, _, _) = encoding_rs::EUC_KR.encode(korean);
        let html = {
            let mut p = Vec::new();
            p.extend_from_slice(b"<html><head><title>Test</title></head><body><p>");
            p.extend_from_slice(&euckr_bytes);
            p.extend_from_slice(b"</p></body></html>");
            p
        };
        let decoded = decode(&html, "text/html");
        assert!(decoded.contains("안녕하세요"), "decoded: {decoded}");
    }

    // ── Japanese encoding detection ──

    #[test]
    fn stat_detect_shiftjis_no_meta() {
        let japanese = "これは日本語のテキストです。機械学習の分野。";
        let (sjis_bytes, _, had_unmappable) = encoding_rs::SHIFT_JIS.encode(japanese);
        assert!(!had_unmappable, "Shift-JIS test text has unmappable chars");
        let html = {
            let mut p = Vec::new();
            p.extend_from_slice(b"<html><head><title>Test</title></head><body><p>");
            p.extend_from_slice(&sjis_bytes);
            p.extend_from_slice(b"</p></body></html>");
            p
        };
        let enc = statistical_detect(&html);
        assert_eq!(enc, Some(encoding_rs::SHIFT_JIS), "expected Shift-JIS");
    }

    #[test]
    fn decode_shiftjis_no_charset() {
        let japanese = "これは日本語のテキストです。機械学習の分野。";
        let (sjis_bytes, _, _) = encoding_rs::SHIFT_JIS.encode(japanese);
        let html = {
            let mut p = Vec::new();
            p.extend_from_slice(b"<html><head><title>Test</title></head><body><p>");
            p.extend_from_slice(&sjis_bytes);
            p.extend_from_slice(b"</p></body></html>");
            p
        };
        let decoded = decode(&html, "text/html");
        assert!(decoded.contains("日本語"), "decoded: {decoded}");
        assert!(decoded.contains("機械学習"), "decoded: {decoded}");
    }

    #[test]
    fn decode_shiftjis_with_meta() {
        let japanese = "これは日本語のテキストです。";
        let (sjis_bytes, _, _) = encoding_rs::SHIFT_JIS.encode(japanese);
        let html = {
            let mut p = Vec::new();
            p.extend_from_slice(b"<html><head><meta charset=\"shift_jis\"></head><body><p>");
            p.extend_from_slice(&sjis_bytes);
            p.extend_from_slice(b"</p></body></html>");
            p
        };
        let decoded = decode(&html, "text/html");
        assert!(decoded.contains("日本語"), "decoded: {decoded}");
    }

    #[test]
    fn decode_eucjp_no_charset() {
        let japanese = "これは日本語のテキストです。機械学習の分野。";
        let (eucjp_bytes, _, _) = encoding_rs::EUC_JP.encode(japanese);
        let html = {
            let mut p = Vec::new();
            p.extend_from_slice(b"<html><head><title>Test</title></head><body><p>");
            p.extend_from_slice(&eucjp_bytes);
            p.extend_from_slice(b"</p></body></html>");
            p
        };
        let decoded = decode(&html, "text/html");
        assert!(decoded.contains("日本語"), "decoded: {decoded}");
    }

    // ── Declared charsets only (text, feeds) ──

    #[test]
    fn declared_bom_then_header_then_xml_declaration_then_utf8() {
        let ru = "Привет";
        let (cp1251, _, _) = encoding_rs::WINDOWS_1251.encode(ru);
        let with_decl = |label: &str| {
            let mut xml = format!("<?xml version='1.0' encoding='{label}'?><p>").into_bytes();
            xml.extend_from_slice(&cp1251);
            xml
        };
        let xml = with_decl("windows-1251");
        assert!(decode_declared(&xml, "application/xml").contains(ru));
        let xml = with_decl("utf-8");
        assert!(decode_declared(&xml, "text/xml; charset=windows-1251").contains(ru));
        let bom = [b"\xEF\xBB\xBF".as_slice(), ru.as_bytes()].concat();
        assert_eq!(
            decode_declared(&bom, "text/plain; charset=windows-1251"),
            ru
        );
        let text = "Größe, charset=iso-8859-1";
        assert_eq!(decode_declared(text.as_bytes(), "text/plain"), text);
    }

    #[test]
    fn a_declared_utf16_or_replacement_label_in_ascii_bytes_decodes_as_utf8() {
        for label in ["UTF-16", "iso-2022-kr"] {
            let xml = format!("<?xml version=\"1.0\" encoding=\"{label}\"?><t>Größe</t>");
            assert!(
                decode_declared(xml.as_bytes(), "").contains("Größe"),
                "{label}"
            );
        }
    }

    #[test]
    fn a_replacement_header_charset_is_ignored() {
        for label in ["iso-2022-kr", "hz-gb-2312"] {
            let ct = format!("text/plain; charset={label}");
            assert_eq!(decode_declared(b"hello", &ct), "hello", "{label}");
        }
        let (xml, _, _) = encoding_rs::WINDOWS_1251
            .encode("<?xml version=\"1.0\" encoding=\"windows-1251\"?><t>Привет</t>");
        assert!(decode_declared(&xml, "text/xml; charset=iso-2022-kr").contains("Привет"));
    }

    // ── Ghost (browser-provided) text: pinned UTF-8 (#35) ──
    // The browser already decoded the page; CDP returns UTF-8. The
    // rendered DOM keeps the original <meta charset=gb18030> tag, so a
    // bare "text/html" makes sniff_meta re-decode UTF-8 bytes as
    // GB18030 → mojibake (末日乐园 → 鏈棩涔愬洯 on 69shuba).

    fn ghost_dom(text: &str, meta_charset: &str) -> Vec<u8> {
        format!(
            "<html><head><meta charset=\"{meta_charset}\"></head><body><h1>{text}</h1></body></html>"
        )
        .into_bytes()
    }

    #[test]
    fn ghost_ct_pins_utf8_despite_gb18030_meta() {
        // What Chromium's DOM.getOuterHTML returns for a gb18030 page:
        // correct UTF-8 text, stale gb18030 meta declaration.
        let dom = ghost_dom("末日乐园 第1706章 (本章完)", "gb18030");
        let decoded = decode(&dom, GHOST_TEXT_CT);
        assert_eq!(decoded, String::from_utf8(dom.clone()).unwrap());
        assert!(decoded.contains("末日乐园"), "decoded: {decoded}");
        assert!(decoded.contains("(本章完)"), "decoded: {decoded}");
    }

    #[test]
    fn ghost_ct_pins_utf8_gbk_meta() {
        let dom = ghost_dom("机器学习是人工智能的一个分支领域。", "gbk");
        let decoded = decode(&dom, GHOST_TEXT_CT);
        assert!(decoded.contains("机器学习"), "decoded: {decoded}");
        assert!(!decoded.contains('\u{FFFD}'), "no tofu: {decoded}");
    }

    #[test]
    fn ghost_ct_pins_utf8_big5_meta() {
        let dom = ghost_dom("機器學習是人工智能的一個分支領域。", "big5");
        let decoded = decode(&dom, GHOST_TEXT_CT);
        assert!(decoded.contains("機器學習"), "decoded: {decoded}");
    }

    #[test]
    fn ghost_ct_is_valid_utf8_roundtrip() {
        // GHOST_TEXT_CT must never mangle any valid-UTF-8 DOM,
        // including emoji and mixed scripts.
        let text = "mixed 混合 テスト 한글 café 🚀 ✓";
        let dom = ghost_dom(text, "utf-8");
        assert_eq!(decode(&dom, GHOST_TEXT_CT), String::from_utf8(dom).unwrap());
    }

    #[test]
    fn bare_text_html_would_double_decode() {
        // Documents the trap: bare "text/html" on browser DOM hits
        // sniff_meta and mangles the already-decoded text. If this
        // ever fails because decode() changed, re-check the ghost
        // call sites still pin GHOST_TEXT_CT.
        let dom = ghost_dom("末日乐园", "gb18030");
        let decoded = decode(&dom, "text/html");
        assert!(
            !decoded.contains("末日乐园"),
            "bare text/html must not pass browser DOM through cleanly : \
             that is exactly the bug GHOST_TEXT_CT exists to prevent; got: {decoded}"
        );
    }
}
