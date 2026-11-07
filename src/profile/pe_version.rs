//! A Windows executable's own version resource, read without running it.
//!
//! `chrome.exe --version` does not answer on Windows: the binary is a GUI
//! subsystem program, so the flag neither prints nor exits (measured on a
//! GitHub Windows runner, 2026-11-07: eight seconds, killed, with and without
//! `--headless=new`). Every spawn probe therefore burned its whole timeout, on
//! every CLI command and every MCP start. The version the browser claims is
//! already in the file: the `VS_VERSION_INFO` resource that Explorer shows under
//! Properties > Details. Reading it launches nothing, so nothing can hang,
//! wedge or be left behind.
//!
//! The reader is a small, bounds-checked PE walker rather than `GetFileVersionInfo`,
//! so the same code is exercised by the Linux and macOS suites too; every offset it
//! follows is checked, and a malformed file answers `None`.

use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

/// Largest resource section read into memory. A browser's own `.rsrc` is a few
/// hundred KB; anything near this is not a version resource worth trusting.
const MAX_RESOURCE_SECTION: u32 = 32 * 1024 * 1024;

/// `RT_VERSION`.
const RT_VERSION: u32 = 16;

fn u16_at(bytes: &[u8], at: usize) -> Option<u16> {
    let raw = bytes.get(at..at.checked_add(2)?)?;
    Some(u16::from_le_bytes([raw[0], raw[1]]))
}

fn u32_at(bytes: &[u8], at: usize) -> Option<u32> {
    let raw = bytes.get(at..at.checked_add(4)?)?;
    Some(u32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]))
}

fn align4(n: usize) -> usize {
    n.saturating_add(3) & !3
}

/// One section header: where an RVA lives in the file.
struct Section {
    virtual_address: u32,
    raw_pointer: u32,
    raw_size: u32,
}

/// `(ProductName, ProductVersion)` from the executable's version resource, or
/// `None` when the file is not a PE image or carries no version resource.
pub(crate) fn product_info(path: &Path) -> Option<(String, String)> {
    let mut file = std::fs::File::open(path).ok()?;
    let mut head = [0u8; 64];
    file.read_exact(&mut head).ok()?;
    if &head[..2] != b"MZ" {
        return None;
    }
    let pe_at = u64::from(u32_at(&head, 0x3c)?);
    // PE signature, COFF header (20 bytes), then the optional header.
    let mut coff = [0u8; 24];
    file.seek(SeekFrom::Start(pe_at)).ok()?;
    file.read_exact(&mut coff).ok()?;
    if &coff[..4] != b"PE\0\0" {
        return None;
    }
    let section_count = usize::from(u16_at(&coff, 6)?);
    let optional_size = usize::from(u16_at(&coff, 20)?);
    if section_count == 0 || section_count > 96 || !(96..=1024).contains(&optional_size) {
        return None;
    }
    let mut optional = vec![0u8; optional_size];
    file.read_exact(&mut optional).ok()?;
    // Data directories start after the fixed fields: 96 bytes for PE32, 112 for PE32+.
    let directories = match u16_at(&optional, 0)? {
        0x10b => 96,
        0x20b => 112,
        _ => return None,
    };
    let resource_rva = u32_at(&optional, directories + 2 * 8)?;
    if resource_rva == 0 {
        return None;
    }
    let mut table = vec![0u8; section_count * 40];
    file.read_exact(&mut table).ok()?;
    let sections: Vec<Section> = table
        .as_chunks::<40>()
        .0
        .iter()
        .filter_map(|entry| {
            Some(Section {
                virtual_address: u32_at(entry, 12)?,
                raw_size: u32_at(entry, 16)?,
                raw_pointer: u32_at(entry, 20)?,
            })
        })
        .collect();
    // The whole section that holds the resource tree: directory offsets are
    // relative to the tree's start, but a leaf's data is addressed by RVA.
    let section = sections.iter().find(|s| {
        resource_rva >= s.virtual_address
            && u64::from(resource_rva) < u64::from(s.virtual_address) + u64::from(s.raw_size)
    })?;
    if section.raw_size == 0 || section.raw_size > MAX_RESOURCE_SECTION {
        return None;
    }
    let mut rsrc = vec![0u8; section.raw_size as usize];
    file.seek(SeekFrom::Start(u64::from(section.raw_pointer)))
        .ok()?;
    file.read_exact(&mut rsrc).ok()?;

    let tree = (resource_rva - section.virtual_address) as usize;
    let blob = version_blob(&rsrc, tree, section.virtual_address)?;
    let strings = version_strings(blob)?;
    let version = strings
        .get("ProductVersion")
        .or_else(|| strings.get("FileVersion"))
        .cloned()
        .or_else(|| fixed_product_version(blob))?;
    let name = strings.get("ProductName").cloned().unwrap_or_default();
    Some((name, version))
}

/// The banner a browser's `--version` prints, built from the resource:
/// `Google Chrome 156.0.8078.80`.
pub(crate) fn banner(path: &Path) -> Option<String> {
    let (name, version) = product_info(path)?;
    Some(if name.is_empty() {
        version
    } else {
        format!("{name} {version}")
    })
}

/// The `RT_VERSION` leaf: type 16, then the first name, then the first language.
fn version_blob(rsrc: &[u8], tree: usize, section_rva: u32) -> Option<&[u8]> {
    // A directory is 16 bytes of header and 8 bytes per entry; an entry's high
    // bit marks a subdirectory, the rest is an offset from the tree's start.
    let entries = |dir: usize| -> Option<Vec<(u32, u32)>> {
        let named = usize::from(u16_at(rsrc, dir.checked_add(12)?)?);
        let ids = usize::from(u16_at(rsrc, dir.checked_add(14)?)?);
        let count = named.checked_add(ids)?;
        if count > 4096 {
            return None;
        }
        (0..count)
            .map(|i| {
                let at = dir.checked_add(16)?.checked_add(i.checked_mul(8)?)?;
                Some((u32_at(rsrc, at)?, u32_at(rsrc, at + 4)?))
            })
            .collect()
    };
    let descend = |offset: u32| -> Option<usize> {
        (offset & 0x8000_0000 != 0).then(|| tree + (offset & 0x7fff_ffff) as usize)
    };

    let (_, versions) = entries(tree)?
        .into_iter()
        .find(|&(id, _)| id == RT_VERSION)?;
    let names = descend(versions)?;
    let (_, name) = *entries(names)?.first()?;
    let languages = descend(name)?;
    let (_, leaf) = *entries(languages)?.first()?;
    if leaf & 0x8000_0000 != 0 {
        return None;
    }
    let data = tree.checked_add(leaf as usize)?;
    let rva = u32_at(rsrc, data)?;
    let size = u32_at(rsrc, data + 4)? as usize;
    let start = rva.checked_sub(section_rva)? as usize;
    rsrc.get(start..start.checked_add(size)?)
}

/// One `VS_VERSIONINFO`-style block: its key, its value bytes and its children.
struct Block<'a> {
    key: String,
    value: &'a [u8],
    children: &'a [u8],
    /// Distance to the next sibling, padded to a DWORD.
    advance: usize,
}

fn block(bytes: &[u8]) -> Option<Block<'_>> {
    let length = usize::from(u16_at(bytes, 0)?);
    let value_length = usize::from(u16_at(bytes, 2)?);
    let text = u16_at(bytes, 4)? == 1;
    if length < 6 || length > bytes.len() {
        return None;
    }
    let body = &bytes[..length];
    let mut at = 6;
    let mut key = Vec::new();
    loop {
        let unit = u16_at(body, at)?;
        at += 2;
        if unit == 0 {
            break;
        }
        key.push(unit);
    }
    at = align4(at);
    // A text value is measured in characters by most linkers and in bytes by
    // some, so its terminator is the real end and the declared length only
    // bounds the scan. A text block with no value (a string table) has none to
    // scan: what follows its key is its first child.
    let (value_end, after) = if !text {
        let end = at.checked_add(value_length)?;
        (end, align4(end))
    } else if value_length == 0 {
        (at, at)
    } else {
        let limit = at
            .saturating_add(value_length.saturating_mul(2))
            .saturating_add(2)
            .min(body.len());
        let mut end = at;
        while end + 1 < limit && u16_at(body, end)? != 0 {
            end += 2;
        }
        (end, align4(end.saturating_add(2)))
    };
    let value = body.get(at.min(body.len())..value_end.min(body.len()))?;
    Some(Block {
        key: String::from_utf16_lossy(&key),
        value,
        children: body.get(after.min(body.len())..)?,
        advance: align4(length),
    })
}

fn blocks(mut bytes: &[u8]) -> Vec<Block<'_>> {
    let mut found = Vec::new();
    while let Some(next) = block(bytes) {
        let advance = next.advance;
        found.push(next);
        match bytes.get(advance..) {
            Some(rest) if rest.len() >= 6 && found.len() < 256 => bytes = rest,
            _ => break,
        }
    }
    found
}

fn text_of(value: &[u8]) -> String {
    let units: Vec<u16> = value
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| u16::from_le_bytes(*pair))
        .take_while(|&unit| unit != 0)
        .collect();
    String::from_utf16_lossy(&units).trim().to_string()
}

/// Every `StringFileInfo` entry of the first string table, by name.
fn version_strings(blob: &[u8]) -> Option<std::collections::HashMap<String, String>> {
    let root = block(blob)?;
    if root.key != "VS_VERSION_INFO" {
        return None;
    }
    let mut strings = std::collections::HashMap::new();
    for info in blocks(root.children) {
        if info.key != "StringFileInfo" {
            continue;
        }
        if let Some(table) = blocks(info.children).into_iter().next() {
            for entry in blocks(table.children) {
                strings.insert(entry.key, text_of(entry.value));
            }
        }
        break;
    }
    Some(strings)
}

/// `dwProductVersionMS/LS` from the fixed block, for files with no string table.
fn fixed_product_version(blob: &[u8]) -> Option<String> {
    let root = block(blob)?;
    if u32_at(root.value, 0)? != 0xfeef_04bd {
        return None;
    }
    let ms = u32_at(root.value, 16)?;
    let ls = u32_at(root.value, 20)?;
    Some(format!(
        "{}.{}.{}.{}",
        ms >> 16,
        ms & 0xffff,
        ls >> 16,
        ls & 0xffff
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wide(text: &str) -> Vec<u8> {
        text.encode_utf16()
            .chain(std::iter::once(0))
            .flat_map(u16::to_le_bytes)
            .collect()
    }

    fn pad4(bytes: &mut Vec<u8>) {
        while !bytes.len().is_multiple_of(4) {
            bytes.push(0);
        }
    }

    /// One block: length, value length, type, key, padding, value, padding, children.
    fn make_block(
        key: &str,
        kind: u16,
        value_length: u16,
        value: &[u8],
        children: &[u8],
    ) -> Vec<u8> {
        let mut out = vec![0, 0];
        out.extend(value_length.to_le_bytes());
        out.extend(kind.to_le_bytes());
        out.extend(wide(key));
        pad4(&mut out);
        out.extend(value);
        pad4(&mut out);
        out.extend(children);
        let length = out.len() as u16;
        out[..2].copy_from_slice(&length.to_le_bytes());
        pad4(&mut out);
        out
    }

    fn string_entry(name: &str, value: &str) -> Vec<u8> {
        let value = wide(value);
        make_block(name, 1, (value.len() / 2) as u16, &value, &[])
    }

    fn fixed_info(major: u32, minor: u32, build: u32, patch: u32) -> Vec<u8> {
        let mut out = 0xfeef_04bdu32.to_le_bytes().to_vec();
        out.extend(0x0001_0000u32.to_le_bytes());
        out.extend(((major << 16) | minor).to_le_bytes()); // file version
        out.extend(((build << 16) | patch).to_le_bytes());
        out.extend(((major << 16) | minor).to_le_bytes()); // product version
        out.extend(((build << 16) | patch).to_le_bytes());
        out.resize(52, 0);
        out
    }

    fn version_resource(strings: &[(&str, &str)], fixed: Vec<u8>) -> Vec<u8> {
        let mut entries = Vec::new();
        for (name, value) in strings {
            entries.extend(string_entry(name, value));
        }
        let table = make_block("040904b0", 1, 0, &[], &entries);
        let info = make_block("StringFileInfo", 1, 0, &[], &table);
        make_block("VS_VERSION_INFO", 0, 52, &fixed, &info)
    }

    /// A resource directory with a single entry.
    fn directory(id: u32, target: u32, subdirectory: bool) -> Vec<u8> {
        let mut out = vec![0u8; 12];
        out.extend(0u16.to_le_bytes());
        out.extend(1u16.to_le_bytes());
        out.extend(id.to_le_bytes());
        let flag = if subdirectory { 0x8000_0000 } else { 0 };
        out.extend((target | flag).to_le_bytes());
        out
    }

    /// A minimal PE image whose only section is `.rsrc` holding `blob` as RT_VERSION.
    fn image(blob: &[u8], plus: bool) -> Vec<u8> {
        const SECTION_RVA: u32 = 0x3000;
        let mut tree = directory(RT_VERSION, 24, true); // 0: types
        tree.extend(directory(1, 48, true)); // 24: names
        tree.extend(directory(0x409, 72, false)); // 48: languages
        let data_at = tree.len() + 16; // 72: the data entry, then the blob
        tree.extend((SECTION_RVA + data_at as u32).to_le_bytes());
        tree.extend((blob.len() as u32).to_le_bytes());
        tree.extend([0u8; 8]);
        tree.extend(blob);

        let optional_size = if plus { 240 } else { 224 };
        let mut optional = vec![0u8; optional_size];
        optional[..2].copy_from_slice(&(if plus { 0x20bu16 } else { 0x10bu16 }).to_le_bytes());
        let directories = if plus { 112 } else { 96 };
        optional[directories + 16..directories + 20].copy_from_slice(&SECTION_RVA.to_le_bytes());
        optional[directories + 20..directories + 24]
            .copy_from_slice(&(tree.len() as u32).to_le_bytes());

        let mut file = vec![0u8; 0x80];
        file[..2].copy_from_slice(b"MZ");
        file[0x3c..0x40].copy_from_slice(&0x80u32.to_le_bytes());
        file.extend(b"PE\0\0");
        let mut coff = vec![0u8; 20];
        coff[2..4].copy_from_slice(&1u16.to_le_bytes());
        coff[16..18].copy_from_slice(&(optional_size as u16).to_le_bytes());
        file.extend(coff);
        file.extend(optional);
        let mut section = vec![0u8; 40];
        section[..6].copy_from_slice(b".rsrc\0");
        section[8..12].copy_from_slice(&(tree.len() as u32).to_le_bytes());
        section[12..16].copy_from_slice(&SECTION_RVA.to_le_bytes());
        section[16..20].copy_from_slice(&(tree.len() as u32).to_le_bytes());
        let raw_pointer = (file.len() + 40).next_multiple_of(0x200) as u32;
        section[20..24].copy_from_slice(&raw_pointer.to_le_bytes());
        file.extend(section);
        file.resize(raw_pointer as usize, 0);
        file.extend(tree);
        file
    }

    fn written(name: &str, bytes: &[u8]) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("donsetch-pe-{}-{name}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("browser.exe");
        std::fs::write(&path, bytes).unwrap();
        path
    }

    #[test]
    fn a_browsers_banner_comes_from_its_own_version_resource() {
        let resource = version_resource(
            &[
                ("CompanyName", "Google LLC"),
                ("ProductName", "Google Chrome"),
                ("ProductVersion", "156.0.8078.80"),
            ],
            fixed_info(156, 0, 8078, 80),
        );
        for plus in [true, false] {
            let path = written(&format!("chrome-{plus}"), &image(&resource, plus));
            assert_eq!(
                banner(&path).as_deref(),
                Some("Google Chrome 156.0.8078.80")
            );
            assert_eq!(
                crate::profile::parse_version_major(&banner(&path).unwrap()),
                Some(156)
            );
        }
    }

    #[test]
    fn a_brand_that_is_not_google_chrome_is_named_as_itself() {
        let resource = version_resource(
            &[
                ("ProductName", "Microsoft Edge"),
                ("ProductVersion", "156.0.4314.58"),
            ],
            fixed_info(156, 0, 4314, 58),
        );
        let path = written("edge", &image(&resource, true));
        let banner = banner(&path).unwrap();
        assert_eq!(banner, "Microsoft Edge 156.0.4314.58");
        assert!(
            !banner.contains("Google Chrome"),
            "a fork must not claim the brand"
        );
    }

    #[test]
    fn a_file_with_no_string_table_falls_back_to_the_fixed_version() {
        let resource = make_block("VS_VERSION_INFO", 0, 52, &fixed_info(142, 0, 7444, 60), &[]);
        let path = written("fixed", &image(&resource, true));
        assert_eq!(banner(&path).as_deref(), Some("142.0.7444.60"));
    }

    #[test]
    fn what_is_not_a_readable_pe_answers_none_and_never_panics() {
        let good = image(
            &version_resource(&[("ProductVersion", "150.0.1.2")], fixed_info(150, 0, 1, 2)),
            true,
        );
        assert!(banner(&written("good", &good)).is_some());
        // Every truncation, and a corrupted copy for every byte, must be refused
        // or read, never indexed out of bounds.
        for cut in (0..good.len()).step_by(7) {
            let _ = banner(&written("cut", &good[..cut]));
        }
        for at in (0..good.len()).step_by(5) {
            let mut bad = good.clone();
            bad[at] ^= 0xff;
            let _ = banner(&written("bad", &bad));
        }
        assert_eq!(
            banner(&written("text", b"#!/bin/sh\necho Chromium 1.2.3.4\n")),
            None
        );
        assert_eq!(banner(Path::new("/definitely/not/a/browser.exe")), None);
    }

    /// A real, shipped PE: the vendored ONNX runtime, when this checkout has it.
    #[test]
    fn a_real_dll_reads_a_dotted_version() {
        let dll = Path::new(env!("CARGO_MANIFEST_DIR")).join("vendor/onnx/onnxruntime.dll");
        if !dll.exists() {
            return;
        }
        let (_, version) = product_info(&dll).expect("onnxruntime.dll carries a version resource");
        // The runtime's own string is not purely numeric (a commit suffix), so
        // the assertion is its shape: a leading number and several dotted parts.
        assert!(
            version.starts_with(|c: char| c.is_ascii_digit()) && version.split('.').count() >= 3,
            "not a dotted version: {version}"
        );
    }

    /// The Windows lane reads a binary every Windows install has.
    #[cfg(windows)]
    #[test]
    fn windows_own_kernel32_reads_a_windows_version() {
        let root = std::env::var_os("SystemRoot").expect("SystemRoot");
        let kernel = Path::new(&root).join("System32").join("kernel32.dll");
        let (name, version) =
            product_info(&kernel).expect("kernel32.dll carries a version resource");
        assert!(name.contains("Windows"), "ProductName: {name}");
        assert!(version.starts_with("10."), "ProductVersion: {version}");
    }
}
