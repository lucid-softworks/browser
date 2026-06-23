//! A strict, dependency-light WOFF2 → SFNT decoder.
//!
//! WOFF2 (`wOF2`) wraps an OpenType/TrueType font in a single Brotli stream plus a compact table
//! directory. A browser must DECODE a well-formed WOFF2 (so the page's web font renders) and REJECT a
//! malformed one (so it falls back) — the WPT `css/WOFF2` reftests check both halves. This decoder is
//! deliberately **conservative**: anything that doesn't validate cleanly returns `None`, so the
//! caller falls back to the next `@font-face` source. A malformed file is therefore never accepted
//! (the rejection reftests keep passing); the cost is that a valid file using a feature we don't
//! reconstruct also falls back rather than risking a mis-render.
//!
//! Supported: untransformed tables (CFF- and glyf-flavored), plus the spec's `glyf`/`loca` and
//! `hmtx` table transforms (§5.1/§5.3). NOT supported and therefore rejected: `ttcf` font
//! collections.
//!
//! (A handful of `css/WOFF2` reftests render a TrueType-flavored test font against a CFF-flavored
//! reference, or vice versa; those compare a quadratic outline to a cubic one and so hinge on
//! rasterizer fidelity, not on this decoder — the reconstructed glyph is byte-identical to the
//! same-format reference.)
//!
//! Spec: <https://www.w3.org/TR/WOFF2/>. The transformed-`glyf` triplet decoding follows the
//! reference implementation (Google `woff2`, `src/woff2_dec.cc`).

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

/// The kind of (non-null) table transform a directory entry carries.
#[derive(PartialEq, Clone, Copy)]
enum Transform {
    None,
    Glyf, // the paired glyf+loca transform; carried on the glyf entry
    Loca, // the loca half (its bytes are produced while reconstructing glyf)
    Hmtx,
}

/// A parsed table-directory entry.
struct TableEntry {
    tag: [u8; 4],
    orig_length: u32,
    /// Bytes this table occupies in the decompressed stream (== `orig_length` when untransformed).
    transform_length: u32,
    transform: Transform,
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

        // A non-null transform is present unless the version is the "null" value for the tag (3 for
        // glyf/loca, 0 for everything else). Transformed tables carry a second UIntBase128
        // (transformLength); untransformed ones occupy `orig_length` stream bytes. An out-of-range
        // version rejects the font (the `tabledata-transform-bad-flag` fonts).
        let (transform, transform_length) = match (&tag, transform_version) {
            (b"glyf", 0) => (Transform::Glyf, read_base128(data, &mut off)?),
            (b"loca", 0) => (Transform::Loca, read_base128(data, &mut off)?),
            (b"glyf" | b"loca", 3) => (Transform::None, orig_length),
            (b"glyf" | b"loca", _) => return None,
            (b"hmtx", 1) => (Transform::Hmtx, read_base128(data, &mut off)?),
            (b"hmtx", 0) => (Transform::None, orig_length),
            (b"hmtx", _) => return None,
            (_, 0) => (Transform::None, orig_length),
            (_, _) => return None,
        };
        total_len = total_len.checked_add(transform_length as usize)?;
        entries.push(TableEntry {
            tag,
            orig_length,
            transform_length,
            transform,
        });
    }
    let dir_end = off;

    // ---- Block layout validation (spec §3: blocks are tightly packed, 4-byte aligned) -------
    // The Brotli stream occupies [dir_end, dir_end + totalCompressedSize); optional metadata and
    // private-data blocks follow at 4-byte-aligned offsets, and nothing else may appear. The
    // compressed block is padded to a 4-byte boundary so any following block starts aligned; the
    // FINAL block is not padded. `expected_end` is the exact file end and must equal the real length —
    // no extraneous trailing bytes, no overlap-induced shortfall. Rejects `blocks-extraneous-data`
    // and `blocks-overlap`.
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
    // The decompressed stream must be EXACTLY the concatenated (transformed) tables — no more, no
    // less. Rejects the `tabledata-decompressed-length` fonts.
    if stream.len() != total_len {
        return None;
    }

    reconstruct(flavor, &entries, &stream)
}

/// Slice each table from the decompressed `stream`, reconstructing the `glyf`/`loca`/`hmtx`
/// transforms, then assemble the SFNT.
fn reconstruct(flavor: u32, entries: &[TableEntry], stream: &[u8]) -> Option<Vec<u8>> {
    // First pass: copy untransformed tables verbatim and reconstruct glyf (which also produces the
    // loca bytes and per-glyph xMins needed by the loca/hmtx transforms). Each table consumes its
    // `transform_length` bytes from the stream, in directory order.
    let mut tables: Vec<([u8; 4], Vec<u8>)> = Vec::with_capacity(entries.len());
    let mut cursor = 0usize;
    let mut loca_bytes: Option<Vec<u8>> = None;
    let mut x_mins: Vec<i16> = Vec::new();
    let mut num_glyphs_from_glyf = 0usize;
    // Remember the directory slots of the transformed loca/hmtx tables to fill on the second pass.
    let mut loca_slot: Option<usize> = None;
    let mut hmtx_slot: Option<(usize, Vec<u8>)> = None;
    for e in entries {
        let slice = stream.get(cursor..cursor + e.transform_length as usize)?;
        cursor += e.transform_length as usize;
        match e.transform {
            Transform::None => tables.push((e.tag, slice.to_vec())),
            Transform::Glyf => {
                let g = reconstruct_glyf(slice)?;
                num_glyphs_from_glyf = g.num_glyphs;
                x_mins = g.x_mins;
                loca_bytes = Some(g.loca);
                tables.push((e.tag, g.glyf));
            }
            Transform::Loca => {
                // The loca bytes come from the glyf transform; the loca half must itself carry no data
                // (transformLength 0). Rejects `tabledata-non-zero-loca`.
                if e.transform_length != 0 {
                    return None;
                }
                loca_slot = Some(tables.len());
                tables.push((e.tag, Vec::new()));
            }
            Transform::Hmtx => {
                hmtx_slot = Some((tables.len(), slice.to_vec()));
                tables.push((e.tag, Vec::new()));
            }
        }
    }

    // Second pass: fill the transformed loca and hmtx slots, which depend on the reconstructed glyf.
    let loca = loca_bytes;
    if let Some(slot) = loca_slot {
        let loca = loca.as_ref()?; // a transformed loca with no transformed glyf is invalid
                                   // The directory's loca origLength must match the reconstructed table exactly. Rejects the
                                   // `tabledata-bad-origlength-loca` fonts.
        if entries.get(slot)?.orig_length as usize != loca.len() {
            return None;
        }
        tables[slot].1 = loca.clone();
    } else if loca.is_some() {
        // A transformed glyf requires a transformed loca slot to fill.
        return None;
    }
    if let Some((slot, transformed)) = hmtx_slot {
        let num_hmetrics = num_h_metrics(&tables)?;
        let hmtx = reconstruct_hmtx(&transformed, num_glyphs_from_glyf, num_hmetrics, &x_mins)?;
        tables[slot].1 = hmtx;
    }

    let refs: Vec<([u8; 4], &[u8])> = tables.iter().map(|(t, b)| (*t, b.as_slice())).collect();
    Some(assemble_sfnt(flavor, &refs))
}

/// Look up `numberOfHMetrics` from the (untransformed) `hhea` table already placed in `tables`.
fn num_h_metrics(tables: &[([u8; 4], Vec<u8>)]) -> Option<u16> {
    let hhea = &tables.iter().find(|(t, _)| t == b"hhea")?.1;
    read_u16(hhea, 34) // numberOfHMetrics is the last field of hhea
}

/// Output of the transformed-`glyf` reconstruction.
struct Glyf {
    glyf: Vec<u8>,
    loca: Vec<u8>,
    x_mins: Vec<i16>,
    num_glyphs: usize,
}

/// Reconstruct the `glyf` and `loca` tables from the WOFF2 transformed-`glyf` representation
/// (spec §5.1). Follows the reference decoder's stream layout and triplet scheme.
fn reconstruct_glyf(data: &[u8]) -> Option<Glyf> {
    // Header: version(4) numGlyphs(2) indexFormat(2) then 7 substream sizes (4 each) = 36 bytes.
    let option_flags = read_u16(data, 2)?;
    let num_glyphs = read_u16(data, 4)? as usize;
    let index_format = read_u16(data, 6)?;
    let mut sizes = [0usize; 7];
    for (i, s) in sizes.iter_mut().enumerate() {
        *s = read_u32(data, 8 + i * 4)? as usize;
    }
    // The "overlapSimpleBitmap" optional stream (optionFlags bit 0) is not handled; reject if present
    // so we never mis-slice. The WPT fonts don't use it.
    if option_flags & 0x0001 != 0 {
        return None;
    }
    let mut o = 36usize;
    let mut take = |len: usize| -> Option<&[u8]> {
        let s = data.get(o..o + len)?;
        o += len;
        Some(s)
    };
    let n_contour = take(sizes[0])?;
    let n_points = take(sizes[1])?;
    let flags = take(sizes[2])?;
    let glyph_stream = take(sizes[3])?;
    let composite = take(sizes[4])?;
    let bbox = take(sizes[5])?;
    let instructions = take(sizes[6])?;
    // The substreams must account for exactly the transformed table (no trailing bytes).
    if o != data.len() {
        return None;
    }
    // The bbox bitmap prefixes the bbox stream, 4-byte (32-bit-word) aligned.
    let bbox_bitmap_len = ((num_glyphs + 31) >> 5) << 2;
    let bbox_bitmap = bbox.get(..bbox_bitmap_len)?;
    let mut b = GlyfBuilder {
        n_contour: Reader::new(n_contour),
        n_points: Reader::new(n_points),
        flags: Reader::new(flags),
        glyph_stream: Reader::new(glyph_stream),
        composite: Reader::new(composite),
        bbox: Reader::new(bbox.get(bbox_bitmap_len..)?),
        bbox_bitmap,
        instructions: Reader::new(instructions),
    };
    b.build(num_glyphs, index_format)
}

/// A simple big-endian byte cursor over one transformed sub-stream.
struct Reader<'a> {
    b: &'a [u8],
    pos: usize,
}
impl<'a> Reader<'a> {
    fn new(b: &'a [u8]) -> Self {
        Reader { b, pos: 0 }
    }
    fn u8(&mut self) -> Option<u8> {
        let v = *self.b.get(self.pos)?;
        self.pos += 1;
        Some(v)
    }
    fn i16(&mut self) -> Option<i16> {
        Some(self.u16()? as i16)
    }
    fn u16(&mut self) -> Option<u16> {
        let v = read_u16(self.b, self.pos)?;
        self.pos += 2;
        Some(v)
    }
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let s = self.b.get(self.pos..self.pos + n)?;
        self.pos += n;
        Some(s)
    }
    /// Read a 255UInt16 (spec §4.2), used for points-per-contour and instruction lengths.
    fn u255(&mut self) -> Option<u16> {
        match self.u8()? {
            253 => self.u16(),
            254 => Some(self.u8()? as u16 + 253 * 2),
            255 => Some(self.u8()? as u16 + 253),
            code => Some(code as u16),
        }
    }
}

/// Holds the transformed-`glyf` sub-streams while rebuilding outlines.
struct GlyfBuilder<'a> {
    n_contour: Reader<'a>,
    n_points: Reader<'a>,
    flags: Reader<'a>,
    glyph_stream: Reader<'a>,
    composite: Reader<'a>,
    bbox: Reader<'a>,
    bbox_bitmap: &'a [u8],
    instructions: Reader<'a>,
}

impl GlyfBuilder<'_> {
    fn build(&mut self, num_glyphs: usize, index_format: u16) -> Option<Glyf> {
        let mut glyf: Vec<u8> = Vec::new();
        let mut offsets: Vec<u32> = Vec::with_capacity(num_glyphs + 1);
        let mut x_mins: Vec<i16> = Vec::with_capacity(num_glyphs);
        offsets.push(0);
        for gid in 0..num_glyphs {
            let has_bbox = self
                .bbox_bitmap
                .get(gid >> 3)
                .map(|byte| (byte >> (7 - (gid & 7))) & 1 == 1)
                .unwrap_or(false);
            let n_contours = self.n_contour.u16()?;
            let glyph = if n_contours == 0 {
                // Empty glyph: must NOT carry a bbox (rejects `tabledata-glyf-bbox-003`).
                if has_bbox {
                    return None;
                }
                x_mins.push(0);
                Vec::new()
            } else if n_contours == 0xffff {
                // Composite glyph: a bbox is mandatory (rejects `tabledata-glyf-bbox-002`).
                if !has_bbox {
                    return None;
                }
                let (bx, glyph) = self.build_composite()?;
                x_mins.push(bx);
                glyph
            } else {
                let (bx, glyph) = self.build_simple(n_contours as usize, has_bbox)?;
                x_mins.push(bx);
                glyph
            };
            glyf.extend_from_slice(&glyph);
            // Pad each glyph to an even length so short-format loca (offset/2) stays exact.
            if glyf.len() % 2 != 0 {
                glyf.push(0);
            }
            offsets.push(glyf.len() as u32);
        }
        let loca = encode_loca(&offsets, index_format)?;
        Some(Glyf {
            glyf,
            loca,
            x_mins,
            num_glyphs,
        })
    }

    /// Build a simple glyph; returns `(xMin, glyf_bytes)`.
    fn build_simple(&mut self, n_contours: usize, has_bbox: bool) -> Option<(i16, Vec<u8>)> {
        let mut end_pts: Vec<u16> = Vec::with_capacity(n_contours);
        let mut total_points: u32 = 0;
        for _ in 0..n_contours {
            total_points = total_points.checked_add(self.n_points.u255()? as u32)?;
            end_pts.push((total_points.checked_sub(1)?) as u16);
        }
        let n = total_points as usize;
        // Read one flag byte per point, then the triplet-encoded deltas.
        let mut on_curve = Vec::with_capacity(n);
        let mut xs: Vec<i16> = Vec::with_capacity(n);
        let mut ys: Vec<i16> = Vec::with_capacity(n);
        let mut x: i32 = 0;
        let mut y: i32 = 0;
        for _ in 0..n {
            let flag = self.flags.u8()?;
            on_curve.push(flag >> 7 == 0);
            let (dx, dy) = self.read_triplet(flag & 0x7f)?;
            x += dx;
            y += dy;
            xs.push(clamp_i16(x));
            ys.push(clamp_i16(y));
        }
        let instruction_length = self.glyph_stream.u255()? as usize;
        let instr = self.instructions.take(instruction_length)?;
        let (xmin, ymin, xmax, ymax) = if has_bbox {
            (
                self.bbox.i16()?,
                self.bbox.i16()?,
                self.bbox.i16()?,
                self.bbox.i16()?,
            )
        } else {
            bbox_of(&xs, &ys)
        };

        let mut g = Vec::new();
        g.extend_from_slice(&(n_contours as i16).to_be_bytes());
        g.extend_from_slice(&xmin.to_be_bytes());
        g.extend_from_slice(&ymin.to_be_bytes());
        g.extend_from_slice(&xmax.to_be_bytes());
        g.extend_from_slice(&ymax.to_be_bytes());
        for ep in &end_pts {
            g.extend_from_slice(&ep.to_be_bytes());
        }
        g.extend_from_slice(&(instruction_length as u16).to_be_bytes());
        g.extend_from_slice(instr);
        encode_point_coords(&mut g, &on_curve, &xs, &ys);
        Some((xmin, g))
    }

    /// Build a composite glyph; returns `(xMin, glyf_bytes)`. Component records are copied verbatim
    /// from the composite stream; instructions (if any) come from the glyph/instruction streams.
    fn build_composite(&mut self) -> Option<(i16, Vec<u8>)> {
        let xmin = self.bbox.i16()?;
        let ymin = self.bbox.i16()?;
        let xmax = self.bbox.i16()?;
        let ymax = self.bbox.i16()?;
        let mut components = Vec::new();
        let mut have_instructions = false;
        loop {
            let flags = self.composite.u16()?;
            let glyph_index = self.composite.u16()?;
            components.extend_from_slice(&flags.to_be_bytes());
            components.extend_from_slice(&glyph_index.to_be_bytes());
            // ARG_1_AND_2_ARE_WORDS → two int16 args, else two int8 args.
            let arg_bytes = if flags & 0x0001 != 0 { 4 } else { 2 };
            // Scale variants: WE_HAVE_A_SCALE / X_AND_Y_SCALE / TWO_BY_TWO.
            let scale_bytes = if flags & 0x0008 != 0 {
                2
            } else if flags & 0x0040 != 0 {
                4
            } else if flags & 0x0080 != 0 {
                8
            } else {
                0
            };
            components.extend_from_slice(self.composite.take(arg_bytes + scale_bytes)?);
            if flags & 0x0100 != 0 {
                have_instructions = true; // WE_HAVE_INSTRUCTIONS
            }
            if flags & 0x0020 == 0 {
                break; // no MORE_COMPONENTS
            }
        }
        let mut g = Vec::new();
        g.extend_from_slice(&(-1i16).to_be_bytes());
        g.extend_from_slice(&xmin.to_be_bytes());
        g.extend_from_slice(&ymin.to_be_bytes());
        g.extend_from_slice(&xmax.to_be_bytes());
        g.extend_from_slice(&ymax.to_be_bytes());
        g.extend_from_slice(&components);
        if have_instructions {
            let instruction_length = self.glyph_stream.u255()? as usize;
            let instr = self.instructions.take(instruction_length)?;
            g.extend_from_slice(&(instruction_length as u16).to_be_bytes());
            g.extend_from_slice(instr);
        }
        Some((xmin, g))
    }

    /// Decode one (dx, dy) delta from the glyph stream given a 7-bit triplet flag (spec §5.2 /
    /// reference `TripletDecode`).
    fn read_triplet(&mut self, flag: u8) -> Option<(i32, i32)> {
        let flag = flag as i32;
        let with_sign = |flag: i32, base: i32| if flag & 1 != 0 { base } else { -base };
        let n = if flag < 84 {
            1
        } else if flag < 120 {
            2
        } else if flag < 124 {
            3
        } else {
            4
        };
        let d = self.glyph_stream.take(n)?;
        let (dx, dy) = if flag < 10 {
            (0, with_sign(flag, ((flag & 14) << 7) + d[0] as i32))
        } else if flag < 20 {
            (with_sign(flag, (((flag - 10) & 14) << 7) + d[0] as i32), 0)
        } else if flag < 84 {
            let b0 = flag - 20;
            let b1 = d[0] as i32;
            (
                with_sign(flag, 1 + (b0 & 0x30) + (b1 >> 4)),
                with_sign(flag >> 1, 1 + ((b0 & 0x0c) << 2) + (b1 & 0x0f)),
            )
        } else if flag < 120 {
            let b0 = flag - 84;
            (
                with_sign(flag, 1 + ((b0 / 12) << 8) + d[0] as i32),
                with_sign(flag >> 1, 1 + (((b0 % 12) >> 2) << 8) + d[1] as i32),
            )
        } else if flag < 124 {
            (
                with_sign(flag, ((d[0] as i32) << 4) + (d[1] as i32 >> 4)),
                with_sign(flag >> 1, ((d[1] as i32 & 0x0f) << 8) + d[2] as i32),
            )
        } else {
            (
                with_sign(flag, ((d[0] as i32) << 8) + d[1] as i32),
                with_sign(flag >> 1, ((d[2] as i32) << 8) + d[3] as i32),
            )
        };
        Some((dx, dy))
    }
}

/// Reconstruct an untransformed `hmtx` from the WOFF2 `hmtx` transform (spec §5.3). The transform
/// drops the left-side-bearing arrays that equal each glyph's xMin; we restore them from `x_mins`.
fn reconstruct_hmtx(
    data: &[u8],
    num_glyphs: usize,
    num_h_metrics: u16,
    x_mins: &[i16],
) -> Option<Vec<u8>> {
    let num_h_metrics = num_h_metrics as usize;
    if num_h_metrics == 0 || num_h_metrics > num_glyphs || x_mins.len() != num_glyphs {
        return None;
    }
    let mut r = Reader::new(data);
    let flags = r.u8()?;
    // Reserved bits (0xFC) must be zero (rejects `tabledata-transform-hmtx-003`).
    if flags & 0xFC != 0 {
        return None;
    }
    let prop_lsb_present = flags & 0x01 == 0;
    let mono_lsb_present = flags & 0x02 == 0;
    // The transform must omit at least one LSB array, else it isn't a transform (rejects the
    // null-transform `tabledata-transform-hmtx-004`, whose flags are 0).
    if prop_lsb_present && mono_lsb_present {
        return None;
    }
    let advances: Vec<u16> = (0..num_h_metrics).map(|_| r.u16()).collect::<Option<_>>()?;
    let prop_lsbs: Vec<i16> = if prop_lsb_present {
        (0..num_h_metrics).map(|_| r.i16()).collect::<Option<_>>()?
    } else {
        Vec::new()
    };
    let mono_lsbs: Vec<i16> = if mono_lsb_present {
        (0..num_glyphs - num_h_metrics)
            .map(|_| r.i16())
            .collect::<Option<_>>()?
    } else {
        Vec::new()
    };
    // The transformed table must be fully consumed (no extraneous trailing bytes).
    if r.pos != data.len() {
        return None;
    }
    let mut out = Vec::with_capacity(num_h_metrics * 4 + (num_glyphs - num_h_metrics) * 2);
    for i in 0..num_h_metrics {
        let lsb = if prop_lsb_present {
            prop_lsbs[i]
        } else {
            x_mins[i]
        };
        out.extend_from_slice(&advances[i].to_be_bytes());
        out.extend_from_slice(&lsb.to_be_bytes());
    }
    for i in num_h_metrics..num_glyphs {
        let lsb = if mono_lsb_present {
            mono_lsbs[i - num_h_metrics]
        } else {
            x_mins[i]
        };
        out.extend_from_slice(&lsb.to_be_bytes());
    }
    Some(out)
}

/// Encode a `loca` table from cumulative glyph offsets, in short (×2, even offsets) or long (×4)
/// format. Short format requires every offset to be even; we reject otherwise.
fn encode_loca(offsets: &[u32], index_format: u16) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(offsets.len() * if index_format == 0 { 2 } else { 4 });
    for &o in offsets {
        if index_format == 0 {
            if o % 2 != 0 || o / 2 > u16::MAX as u32 {
                return None;
            }
            out.extend_from_slice(&((o / 2) as u16).to_be_bytes());
        } else {
            out.extend_from_slice(&o.to_be_bytes());
        }
    }
    Some(out)
}

/// Re-encode a simple glyph's point flags + x/y delta arrays in standard `glyf` form (no repeat
/// compression — valid, just slightly larger than the minimal encoding).
fn encode_point_coords(out: &mut Vec<u8>, on_curve: &[bool], xs: &[i16], ys: &[i16]) {
    let mut flags = Vec::with_capacity(on_curve.len());
    let mut xb = Vec::new();
    let mut yb = Vec::new();
    let mut prev_x = 0i16;
    let mut prev_y = 0i16;
    for i in 0..on_curve.len() {
        let mut flag = if on_curve[i] { 0x01 } else { 0x00 }; // ON_CURVE_POINT
        let dx = xs[i] - prev_x;
        let dy = ys[i] - prev_y;
        prev_x = xs[i];
        prev_y = ys[i];
        if dx == 0 {
            flag |= 0x10; // X_IS_SAME_OR_POSITIVE_X_SHORT (same)
        } else if (-255..=255).contains(&dx) {
            flag |= 0x02; // X_SHORT_VECTOR
            if dx > 0 {
                flag |= 0x10;
            }
            xb.push(dx.unsigned_abs() as u8);
        } else {
            xb.extend_from_slice(&dx.to_be_bytes());
        }
        if dy == 0 {
            flag |= 0x20; // Y_IS_SAME_OR_POSITIVE_Y_SHORT (same)
        } else if (-255..=255).contains(&dy) {
            flag |= 0x04; // Y_SHORT_VECTOR
            if dy > 0 {
                flag |= 0x20;
            }
            yb.push(dy.unsigned_abs() as u8);
        } else {
            yb.extend_from_slice(&dy.to_be_bytes());
        }
        flags.push(flag);
    }
    out.extend_from_slice(&flags);
    out.extend_from_slice(&xb);
    out.extend_from_slice(&yb);
}

/// Bounding box over point arrays (used when a simple glyph's bbox was omitted by the transform).
fn bbox_of(xs: &[i16], ys: &[i16]) -> (i16, i16, i16, i16) {
    (
        xs.iter().copied().min().unwrap_or(0),
        ys.iter().copied().min().unwrap_or(0),
        xs.iter().copied().max().unwrap_or(0),
        ys.iter().copied().max().unwrap_or(0),
    )
}

fn clamp_i16(v: i32) -> i16 {
    v.clamp(i16::MIN as i32, i16::MAX as i32) as i16
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
    let mut entry_selector = 0u16;
    while (1u32 << (entry_selector + 1)) <= u32::from(num) {
        entry_selector += 1;
    }
    let search_range = (1u16 << entry_selector) * 16;
    let range_shift = num.wrapping_mul(16).wrapping_sub(search_range);

    let header_len = 12 + 16 * tables.len();
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

    let mut order: Vec<usize> = (0..tables.len()).collect();
    order.sort_by(|&a, &b| tables[a].0.cmp(&tables[b].0));
    for &i in &order {
        let (tag, bytes) = &tables[i];
        out.extend_from_slice(tag);
        out.extend_from_slice(&checksum(bytes).to_be_bytes());
        out.extend_from_slice(&offsets[i].to_be_bytes());
        out.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
    }
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

    fn woff2(name: &str) -> Option<Vec<u8>> {
        let p = format!(
            "{}/../../wpt/css/WOFF2/support/{name}.woff2",
            env!("CARGO_MANIFEST_DIR")
        );
        std::fs::read(p).ok()
    }

    /// A decoded font must be parseable by our font backend.
    fn assert_decodes(name: &str) {
        if let Some(bytes) = woff2(name) {
            let sfnt = decode(&bytes).unwrap_or_else(|| panic!("{name} should decode"));
            assert!(
                fontdue::Font::from_bytes(sfnt, fontdue::FontSettings::default()).is_ok(),
                "{name} decoded to an unparseable SFNT"
            );
        }
    }

    #[test]
    fn decodes_untransformed_and_transformed_fonts() {
        for name in [
            "valid-001",                     // CFF, untransformed
            "valid-002",                     // CFF with a metadata block
            "valid-005",                     // glyf, null transform
            "directory-knowntags-001",       // custom tags + glyf/hmtx transform
            "tabledata-glyf-bbox-001",       // simple glyph, computed bbox
            "tabledata-glyf-origlength-001", // glyf origLength ignored (too small)
            "tabledata-glyf-origlength-002", // glyf origLength ignored (too big)
            "tabledata-recontruct-loca-001", // composite glyphs, short loca
            "tabledata-transform-hmtx-001",  // transformed hmtx
        ] {
            assert_decodes(name);
        }
    }

    #[test]
    fn rejects_malformed_fonts() {
        for name in [
            "header-signature-001",
            "header-length-001",
            "header-numTables-001",
            "blocks-extraneous-data-001",
            "blocks-overlap-001",
            "datatypes-invalid-base128-001",
            "tabledata-brotli-001",
            "tabledata-extraneous-data-001",
            "tabledata-decompressed-length-001",
            "tabledata-transform-bad-flag-001",
            "tabledata-glyf-bbox-002",           // composite without bbox
            "tabledata-glyf-bbox-003",           // empty glyph with bbox
            "tabledata-non-zero-loca-001",       // loca transformLength != 0
            "tabledata-bad-origlength-loca-001", // loca origLength too small
            "tabledata-bad-origlength-loca-002", // loca origLength too big
            "tabledata-transform-hmtx-003",      // reserved flag bits set
            "tabledata-transform-hmtx-004",      // null hmtx transform
        ] {
            if let Some(bytes) = woff2(name) {
                assert!(decode(&bytes).is_none(), "{name} must be rejected");
            }
        }
    }
}
