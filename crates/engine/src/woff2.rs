//! A strict, dependency-light WOFF2 → SFNT decoder.
//!
//! WOFF2 (`wOF2`) wraps an OpenType/TrueType font in a single Brotli stream plus a compact table
//! directory. A browser must DECODE a well-formed WOFF2 (so the page's web font renders) and REJECT a
//! malformed one (so it falls back) — the WPT `css/WOFF2` reftests check both halves. This decoder is
//! deliberately **conservative**: anything that doesn't validate cleanly returns `None`, so the
//! caller falls back to the next `@font-face` source. That bias means a malformed file is never
//! accepted (the rejection reftests keep passing); the cost is that a valid file using a feature we
//! don't reconstruct also falls back rather than risking a mis-render.
//!
//! Supported: the common case where every table is stored verbatim (untransformed) — this covers
//! CFF-flavored fonts and glyf-flavored fonts encoded with the null transform. NOT yet supported and
//! therefore rejected: the `glyf`/`loca` and `hmtx` table transforms (spec §5.1/§5.3) and `ttcf`
//! font collections. Adding the `glyf`+`hmtx` transforms would unlock the handful of
//! `tabledata-glyf-*` / `tabledata-recontruct-loca` / `directory-knowntags` reftests that use them.
//!
//! Spec: <https://www.w3.org/TR/WOFF2/>.

/// WOFF2 file signature, `wOF2` big-endian.
const WOFF2_SIGNATURE: u32 = 0x774F_4632;
/// `ttcf` — a font collection, which needs a different reconstruction path we don't implement.
const TTC_FLAVOR: u32 = 0x7474_6366;

/// The 63 well-known table tags, indexed by the low 6 bits of a directory entry's flags byte. Index
/// 63 (`0x3f`) is the escape meaning "an explicit 4-byte tag follows". Order is normative (spec §5).
#[rustfmt::skip]
const KNOWN_TAGS: [&[u8; 4]; 63] = [
    b"cmap", b"head", b"hhea", b"hmtx", b"maxp", b"name", b"OS/2", b"post", b"cvt ", b"fpgm",
    b"glyf", b"loca", b"prep", b"CFF ", b"VORG", b"EBDT", b"EBLC", b"gasp", b"hdmx", b"kern",
    b"LTSH", b"PCLT", b"VDMX", b"vhea", b"vmtx", b"BASE", b"GDEF", b"GPOS", b"GSUB", b"EBSC",
    b"JSTF", b"MATH", b"CBDT", b"CBLC", b"COLR", b"CPAL", b"SVG ", b"sbix", b"acnt", b"avar",
    b"bdat", b"bloc", b"bsln", b"cvar", b"fdsc", b"feat", b"fmtx", b"fvar", b"gvar", b"hsty",
    b"just", b"lcar", b"mort", b"morx", b"opbd", b"prop", b"trak", b"Zapf", b"Silf", b"Glat",
    b"Gloc", b"Feat", b"Sill",
];

/// A parsed table-directory entry (untransformed: its stream length equals its SFNT length).
struct TableEntry {
    tag: [u8; 4],
    length: u32,
}

/// Round `n` up to the next multiple of 4 (SFNT/WOFF2 block alignment), saturating.
fn round4(n: usize) -> usize {
    n.saturating_add(3) & !3
}

/// Read a big-endian `u16`/`u32` at `off`, or `None` if out of range.
fn read_u16(b: &[u8], off: usize) -> Option<u16> {
    b.get(off..off + 2)
        .map(|s| u16::from_be_bytes([s[0], s[1]]))
}
fn read_u32(b: &[u8], off: usize) -> Option<u32> {
    b.get(off..off + 4)
        .map(|s| u32::from_be_bytes([s[0], s[1], s[2], s[3]]))
}

/// Decode a `UIntBase128` value at `*off`, advancing `*off`. Strictly per spec §4.1: at most 5
/// bytes, no leading-zero continuation (first byte may not be `0x80`), and no overflow past 32 bits.
/// Returns `None` on any violation — this is what rejects the `datatypes-invalid-base128` fonts.
fn read_base128(b: &[u8], off: &mut usize) -> Option<u32> {
    let mut accum: u32 = 0;
    for i in 0..5 {
        let byte = *b.get(*off)?;
        *off += 1;
        // A leading 0x80 byte encodes a leading zero — not allowed.
        if i == 0 && byte == 0x80 {
            return None;
        }
        // If any of the top 7 bits are already set, the next shift-left-by-7 would overflow 32 bits.
        if accum & 0xFE00_0000 != 0 {
            return None;
        }
        accum = (accum << 7) | (u32::from(byte) & 0x7F);
        if byte & 0x80 == 0 {
            return Some(accum);
        }
    }
    None // 5th byte still had the continuation bit set
}

/// Decode `data` (a complete WOFF2 file) into reconstructed SFNT bytes, or `None` if it is not a
/// well-formed, fully-reconstructable WOFF2.
pub(crate) fn decode(data: &[u8]) -> Option<Vec<u8>> {
    // ---- Header (spec §4) -------------------------------------------------------------------
    if read_u32(data, 0)? != WOFF2_SIGNATURE {
        return None;
    }
    let flavor = read_u32(data, 4)?;
    if flavor == TTC_FLAVOR {
        return None;
    }
    let length = read_u32(data, 8)? as usize;
    let num_tables = read_u16(data, 12)?;
    // `length` must match the actual file size exactly (rejects the `header-length` fonts) and a font
    // with zero tables is invalid (`header-numTables`). The `reserved` field (offset 14) is
    // intentionally NOT validated: the spec says "set to 0" but does not require rejecting a non-zero
    // value, and the `header-reserved` reftest expects such a font to still load.
    if length != data.len() || num_tables == 0 {
        return None;
    }
    let total_compressed_size = read_u32(data, 20)? as usize;
    // offset 16 (totalSfntSize) and 24/26 (major/minorVersion) are not needed for reconstruction.
    // The optional metadata/private blocks live at offsets 28..48; we bounds-check them but, since we
    // do not surface WOFF metadata, never parse their contents (the `metadatadisplay-*` reftests pass
    // precisely because a conforming UA that doesn't display metadata still renders the font).
    let meta_offset = read_u32(data, 28)? as usize;
    let meta_length = read_u32(data, 32)? as usize;
    let priv_offset = read_u32(data, 40)? as usize;
    let priv_length = read_u32(data, 44)? as usize;

    // ---- Table directory (spec §5) ----------------------------------------------------------
    let mut off = 48usize;
    let mut entries: Vec<TableEntry> = Vec::with_capacity(num_tables as usize);
    let mut total_len: usize = 0;
    for _ in 0..num_tables {
        let flags = *data.get(off)?;
        off += 1;
        let tag_index = (flags & 0x3f) as usize;
        let tag: [u8; 4] = if tag_index == 0x3f {
            let raw = data.get(off..off + 4)?;
            off += 4;
            [raw[0], raw[1], raw[2], raw[3]]
        } else {
            **KNOWN_TAGS.get(tag_index)?
        };
        let transform_version = (flags >> 6) & 0x3;
        let orig_length = read_base128(data, &mut off)?;

        // A non-null table transform is present unless the version is the "null" value for this tag
        // (3 for glyf/loca, 0 for everything else). We don't reconstruct transformed tables, so any
        // real transform — and any out-of-range version (the `tabledata-transform-bad-flag` fonts) —
        // rejects the font, leaving it to fall back. The transformed cases also carry a second
        // UIntBase128 (transformLength) which we don't reach.
        let null_transform = match &tag {
            b"glyf" | b"loca" => transform_version == 3,
            _ => transform_version == 0,
        };
        if !null_transform {
            return None;
        }
        total_len = total_len.checked_add(orig_length as usize)?;
        entries.push(TableEntry {
            tag,
            length: orig_length,
        });
    }
    let dir_end = off;

    // ---- Block layout validation (spec §3: blocks are tightly packed, 4-byte aligned) -------
    // The Brotli stream occupies [dir_end, dir_end + totalCompressedSize); optional metadata and
    // private-data blocks follow at 4-byte-aligned offsets, and nothing else may appear. The
    // compressed block is padded to a 4-byte boundary so any following block starts aligned; the
    // FINAL block is not padded. We track `expected_end` as the exact file end and require it to equal
    // the real length — no extraneous trailing bytes, no overlap-induced shortfall. This rejects the
    // `blocks-extraneous-data` and `blocks-overlap` fonts.
    let comp_end = dir_end.checked_add(total_compressed_size)?;
    if comp_end > data.len() {
        return None;
    }
    let after_comp = round4(comp_end);
    let mut expected_end = after_comp;
    if meta_length != 0 {
        if meta_offset != after_comp {
            return None;
        }
        let meta_end = meta_offset.checked_add(meta_length)?;
        if meta_end > data.len() {
            return None;
        }
        if priv_length != 0 {
            let priv_end = priv_offset.checked_add(priv_length)?;
            if priv_offset != round4(meta_end) || priv_end > data.len() {
                return None;
            }
            expected_end = priv_end;
        } else if priv_offset != 0 {
            return None;
        } else {
            expected_end = meta_end;
        }
    } else if meta_offset != 0 {
        return None;
    } else if priv_length != 0 {
        let priv_end = priv_offset.checked_add(priv_length)?;
        if priv_offset != after_comp || priv_end > data.len() {
            return None;
        }
        expected_end = priv_end;
    } else if priv_offset != 0 {
        return None;
    }
    if expected_end != data.len() {
        return None;
    }

    // ---- Decompress the font data (spec §6) -------------------------------------------------
    let compressed = data.get(dir_end..comp_end)?;
    let stream = brotli_decompress_exact(compressed)?;
    // The decompressed stream must be EXACTLY the concatenated tables — no more, no less. This
    // rejects the `tabledata-decompressed-length` fonts.
    if stream.len() != total_len {
        return None;
    }

    // ---- Reassemble the SFNT ----------------------------------------------------------------
    let mut tables: Vec<([u8; 4], &[u8])> = Vec::with_capacity(entries.len());
    let mut cursor = 0usize;
    for e in &entries {
        let slice = stream.get(cursor..cursor + e.length as usize)?;
        cursor += e.length as usize;
        tables.push((e.tag, slice));
    }
    Some(assemble_sfnt(flavor, &tables))
}

/// Brotli-decompress `input`, returning the output only if the WHOLE input was consumed (no trailing
/// extraneous bytes) and the stream decoded cleanly. Catches the `tabledata-extraneous-data` and
/// `tabledata-brotli` fonts.
fn brotli_decompress_exact(input: &[u8]) -> Option<Vec<u8>> {
    use std::io::Read;
    let mut out = Vec::new();
    let mut cursor = std::io::Cursor::new(input);
    let mut dec = brotli::Decompressor::new(&mut cursor, 4096);
    if dec.read_to_end(&mut out).is_err() {
        return None;
    }
    drop(dec);
    // The decoder must have consumed every byte of the compressed block.
    if cursor.position() as usize != input.len() {
        return None;
    }
    Some(out)
}

/// Assemble a valid SFNT: offset table + table directory (records sorted by tag) + 4-byte-padded
/// table data (in the original directory order). Table checksums are filled in; `head`'s
/// `checkSumAdjustment` is left as-is since our font parser does not validate it.
fn assemble_sfnt(flavor: u32, tables: &[([u8; 4], &[u8])]) -> Vec<u8> {
    let num = tables.len() as u16;
    // searchRange / entrySelector / rangeShift per the SFNT spec (largest power of two ≤ numTables).
    let mut entry_selector = 0u16;
    while (1u32 << (entry_selector + 1)) <= u32::from(num) {
        entry_selector += 1;
    }
    let search_range = (1u16 << entry_selector) * 16;
    let range_shift = num.wrapping_mul(16).wrapping_sub(search_range);

    let header_len = 12 + 16 * tables.len();
    // Each table's data offset (4-byte aligned), in directory order.
    let mut offsets = vec![0u32; tables.len()];
    let mut cur = header_len;
    for (i, (_tag, bytes)) in tables.iter().enumerate() {
        offsets[i] = cur as u32;
        cur += round4(bytes.len());
    }

    let mut out = Vec::with_capacity(cur);
    out.extend_from_slice(&flavor.to_be_bytes());
    out.extend_from_slice(&num.to_be_bytes());
    out.extend_from_slice(&search_range.to_be_bytes());
    out.extend_from_slice(&entry_selector.to_be_bytes());
    out.extend_from_slice(&range_shift.to_be_bytes());

    // Table records, sorted by tag (a well-formed SFNT requirement).
    let mut order: Vec<usize> = (0..tables.len()).collect();
    order.sort_by(|&a, &b| tables[a].0.cmp(&tables[b].0));
    for &i in &order {
        let (tag, bytes) = &tables[i];
        out.extend_from_slice(tag);
        out.extend_from_slice(&checksum(bytes).to_be_bytes());
        out.extend_from_slice(&offsets[i].to_be_bytes());
        out.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
    }
    // Table data, in directory order, each padded to a 4-byte boundary.
    for (_tag, bytes) in tables {
        out.extend_from_slice(bytes);
        while out.len() % 4 != 0 {
            out.push(0);
        }
    }
    out
}

/// SFNT table checksum: the wrapping sum of the table's big-endian u32 words (zero-padded).
fn checksum(bytes: &[u8]) -> u32 {
    let mut sum = 0u32;
    let mut i = 0;
    while i < bytes.len() {
        let mut word = [0u8; 4];
        for (j, w) in word.iter_mut().enumerate() {
            if let Some(&b) = bytes.get(i + j) {
                *w = b;
            }
        }
        sum = sum.wrapping_add(u32::from_be_bytes(word));
        i += 4;
    }
    sum
}

#[cfg(test)]
mod tests {
    use super::decode;

    /// Path to a `css/WOFF2` support font in the checked-out WPT tree.
    fn woff2(name: &str) -> Option<Vec<u8>> {
        let p = format!(
            "{}/../../wpt/css/WOFF2/support/{name}.woff2",
            env!("CARGO_MANIFEST_DIR")
        );
        std::fs::read(p).ok()
    }

    #[test]
    fn decodes_a_valid_cff_woff2_to_parseable_sfnt() {
        let Some(bytes) = woff2("valid-001") else {
            return; // WPT tree not present (e.g. minimal CI checkout) — skip.
        };
        let sfnt = decode(&bytes).expect("valid-001 should decode");
        // OTTO (CFF) signature, and fontdue must accept the reconstructed SFNT.
        assert_eq!(&sfnt[0..4], b"OTTO");
        assert!(fontdue::Font::from_bytes(sfnt, fontdue::FontSettings::default()).is_ok());
    }

    #[test]
    fn accepts_a_valid_woff2_with_a_metadata_block() {
        // A font carrying an Extended Metadata block must still decode (we ignore the metadata).
        if let Some(bytes) = woff2("valid-002") {
            assert!(
                decode(&bytes).is_some(),
                "valid-002 (has metadata) should decode"
            );
        }
    }

    #[test]
    fn rejects_malformed_woff2_so_the_caller_falls_back() {
        // A spread of the conformance-rejection fonts: each must fail to decode (→ font-family
        // fallback), which is how the corresponding WPT reftests pass.
        for name in [
            "header-signature-001",              // bad signature
            "header-length-001",                 // header length != file size
            "header-numTables-001",              // zero tables
            "blocks-extraneous-data-001",        // extra bytes between blocks
            "blocks-overlap-001",                // overlapping blocks
            "datatypes-invalid-base128-001",     // illegal UIntBase128
            "tabledata-brotli-001",              // corrupt Brotli stream
            "tabledata-extraneous-data-001",     // trailing bytes in the compressed block
            "tabledata-decompressed-length-001", // wrong decompressed size
            "tabledata-transform-bad-flag-001",  // invalid transform version
        ] {
            if let Some(bytes) = woff2(name) {
                assert!(decode(&bytes).is_none(), "{name} must be rejected");
            }
        }
    }
}
