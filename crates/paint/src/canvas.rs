//! Canvas 2D rasterizer: turns the JS `<canvas>` display lists (pulled via
//! `js::Session::canvas_lists`) into RGBA bitmaps that the engine composites exactly like a decoded
//! `<img>`.
//!
//! The JS context (in `crates/js`) keeps all drawing STATE — the styles, the 2D affine transform,
//! and the current path — and records a list of *resolved* commands: every coordinate is already in
//! the canvas's device-pixel space (the transform was applied in JS) and every color is already a
//! CSS color string (or an encoded gradient). So this module needs no matrix or style math; it just
//! rasterizes:
//!   * `fillRect` / `clearRect` — axis-aligned rect or transformed quad fill / erase.
//!   * `fill`     — scanline (even-odd) polygon fill of the flattened subpaths.
//!   * `stroke`   — each polyline segment drawn as a thick quad of width `lineWidth`.
//!   * `text`     — each glyph rasterized via the system font, aligned by measuring with that font.
//! Colors may be a flat color or a linear/radial gradient (per-pixel stop interpolation).
//!
//! JSON is parsed by a tiny self-contained value parser (the crate has no serde dependency).
//!
//! This module is engine-agnostic: text glyphs come through the [`GlyphRasterizer`] trait (so it
//! needs no concrete font type) and the output is a paint-local [`RasterImage`] (the engine converts
//! it to its own `DecodedImage`).

use std::collections::HashMap;

use crate::{Color, GlyphRasterizer};

/// A straight-alpha RGBA8 raster produced by [`rasterize_canvas`]. `rgba` is `w * h * 4` bytes.
/// (The engine converts this into its own `DecodedImage`; it is identical in layout.)
pub struct RasterImage {
    pub rgba: Vec<u8>,
    pub w: u32,
    pub h: u32,
}

// ----------------------------------------------------------------------------------------------
// JSON value model + parser (minimal; just enough for the canvas display list).
// ----------------------------------------------------------------------------------------------

#[derive(Debug, Clone)]
#[allow(dead_code)] // Bool is parsed for completeness though the canvas list never uses it.
pub enum Json {
    Null,
    Bool(bool),
    Num(f64),
    Str(String),
    Arr(Vec<Json>),
    Obj(HashMap<String, Json>),
}

impl Json {
    fn num(&self) -> f64 {
        match self {
            Json::Num(n) => *n,
            _ => 0.0,
        }
    }
    fn get<'a>(&'a self, key: &str) -> Option<&'a Json> {
        match self {
            Json::Obj(m) => m.get(key),
            _ => None,
        }
    }
    fn as_arr(&self) -> &[Json] {
        match self {
            Json::Arr(v) => v,
            _ => &[],
        }
    }
    fn as_str(&self) -> &str {
        match self {
            Json::Str(s) => s,
            _ => "",
        }
    }
    /// A numeric field, defaulting to `d`.
    fn f(&self, key: &str, d: f64) -> f64 {
        self.get(key).map(|v| v.num()).unwrap_or(d)
    }
}

struct Parser<'a> {
    b: &'a [u8],
    i: usize,
}

impl<'a> Parser<'a> {
    fn new(s: &'a str) -> Self {
        Parser {
            b: s.as_bytes(),
            i: 0,
        }
    }
    fn ws(&mut self) {
        while self.i < self.b.len() && matches!(self.b[self.i], b' ' | b'\t' | b'\n' | b'\r') {
            self.i += 1;
        }
    }
    fn parse(&mut self) -> Option<Json> {
        self.ws();
        if self.i >= self.b.len() {
            return None;
        }
        match self.b[self.i] {
            b'{' => self.obj(),
            b'[' => self.arr(),
            b'"' => self.string().map(Json::Str),
            b't' | b'f' => self.boolean(),
            b'n' => self.null(),
            _ => self.number(),
        }
    }
    fn obj(&mut self) -> Option<Json> {
        self.i += 1; // {
        let mut m = HashMap::new();
        self.ws();
        if self.i < self.b.len() && self.b[self.i] == b'}' {
            self.i += 1;
            return Some(Json::Obj(m));
        }
        loop {
            self.ws();
            let key = self.string()?;
            self.ws();
            if self.i >= self.b.len() || self.b[self.i] != b':' {
                return None;
            }
            self.i += 1; // :
            let val = self.parse()?;
            m.insert(key, val);
            self.ws();
            match self.b.get(self.i)? {
                b',' => {
                    self.i += 1;
                }
                b'}' => {
                    self.i += 1;
                    break;
                }
                _ => return None,
            }
        }
        Some(Json::Obj(m))
    }
    fn arr(&mut self) -> Option<Json> {
        self.i += 1; // [
        let mut v = Vec::new();
        self.ws();
        if self.i < self.b.len() && self.b[self.i] == b']' {
            self.i += 1;
            return Some(Json::Arr(v));
        }
        loop {
            let val = self.parse()?;
            v.push(val);
            self.ws();
            match self.b.get(self.i)? {
                b',' => {
                    self.i += 1;
                }
                b']' => {
                    self.i += 1;
                    break;
                }
                _ => return None,
            }
        }
        Some(Json::Arr(v))
    }
    fn string(&mut self) -> Option<String> {
        if self.b.get(self.i)? != &b'"' {
            return None;
        }
        self.i += 1;
        let mut s = String::new();
        while self.i < self.b.len() {
            let c = self.b[self.i];
            self.i += 1;
            match c {
                b'"' => return Some(s),
                b'\\' => {
                    let e = *self.b.get(self.i)?;
                    self.i += 1;
                    match e {
                        b'"' => s.push('"'),
                        b'\\' => s.push('\\'),
                        b'/' => s.push('/'),
                        b'n' => s.push('\n'),
                        b't' => s.push('\t'),
                        b'r' => s.push('\r'),
                        b'b' => s.push('\u{8}'),
                        b'f' => s.push('\u{c}'),
                        b'u' => {
                            let hex = self.b.get(self.i..self.i + 4)?;
                            self.i += 4;
                            let code =
                                u32::from_str_radix(std::str::from_utf8(hex).ok()?, 16).ok()?;
                            s.push(char::from_u32(code).unwrap_or('\u{fffd}'));
                        }
                        _ => s.push(e as char),
                    }
                }
                _ => {
                    // Copy the (possibly multi-byte UTF-8) char starting at c.
                    if c < 0x80 {
                        s.push(c as char);
                    } else {
                        // Gather the full UTF-8 sequence.
                        let start = self.i - 1;
                        let len = utf8_len(c);
                        let end = (start + len).min(self.b.len());
                        if let Ok(seg) = std::str::from_utf8(&self.b[start..end]) {
                            s.push_str(seg);
                        }
                        self.i = end;
                    }
                }
            }
        }
        None
    }
    fn number(&mut self) -> Option<Json> {
        let start = self.i;
        while self.i < self.b.len()
            && matches!(
                self.b[self.i],
                b'0'..=b'9' | b'-' | b'+' | b'.' | b'e' | b'E'
            )
        {
            self.i += 1;
        }
        let s = std::str::from_utf8(&self.b[start..self.i]).ok()?;
        s.parse::<f64>().ok().map(Json::Num)
    }
    fn boolean(&mut self) -> Option<Json> {
        if self.b[self.i..].starts_with(b"true") {
            self.i += 4;
            Some(Json::Bool(true))
        } else if self.b[self.i..].starts_with(b"false") {
            self.i += 5;
            Some(Json::Bool(false))
        } else {
            None
        }
    }
    fn null(&mut self) -> Option<Json> {
        if self.b[self.i..].starts_with(b"null") {
            self.i += 4;
            Some(Json::Null)
        } else {
            None
        }
    }
}

fn utf8_len(b: u8) -> usize {
    if b >= 0xF0 {
        4
    } else if b >= 0xE0 {
        3
    } else if b >= 0xC0 {
        2
    } else {
        1
    }
}

fn parse_json(s: &str) -> Option<Json> {
    Parser::new(s).parse()
}

// ----------------------------------------------------------------------------------------------
// Display-list model.
// ----------------------------------------------------------------------------------------------

pub struct CanvasList {
    pub id: usize,
    pub width: u32,
    pub height: u32,
    pub commands: Vec<Json>,
}

/// Parse the JSON array `[{id,width,height,commands:[...]}, ...]` from `js::Session::canvas_lists`.
pub fn parse_canvas_lists(json: &str) -> Vec<CanvasList> {
    let root = match parse_json(json) {
        Some(v) => v,
        None => return Vec::new(),
    };
    let mut out = Vec::new();
    for cv in root.as_arr() {
        let id = cv.f("id", -1.0);
        if id < 0.0 {
            continue;
        }
        let width = cv.f("width", 300.0).max(1.0) as u32;
        let height = cv.f("height", 150.0).max(1.0) as u32;
        // Cap the bitmap so a runaway width/height can't allocate gigabytes.
        let width = width.min(8192);
        let height = height.min(8192);
        let commands = cv
            .get("commands")
            .map(|c| c.as_arr().to_vec())
            .unwrap_or_default();
        out.push(CanvasList {
            id: id as usize,
            width,
            height,
            commands,
        });
    }
    out
}

// ----------------------------------------------------------------------------------------------
// A standalone RGBA raster target (separate from paint::Framebuffer so it starts fully TRANSPARENT,
// which is what an empty canvas is). Same straight-alpha source-over compositing.
// ----------------------------------------------------------------------------------------------

struct Canvas {
    w: u32,
    h: u32,
    px: Vec<u8>, // RGBA8, starts transparent (all zero)
    /// Active clip rect in device pixels `(x0, y0, x1, y1)` (exclusive upper bound); `None` = none.
    /// Set per-command from the display-list `clip` field; `blend`/`erase` reject pixels outside it.
    clip: Option<(i32, i32, i32, i32)>,
}

impl Canvas {
    fn new(w: u32, h: u32) -> Self {
        Canvas {
            w,
            h,
            px: vec![0u8; (w as usize) * (h as usize) * 4],
            clip: None,
        }
    }
    /// True if (x,y) is inside the active clip rect (or no clip is set).
    #[inline]
    fn in_clip(&self, x: i32, y: i32) -> bool {
        match self.clip {
            Some((x0, y0, x1, y1)) => x >= x0 && y >= y0 && x < x1 && y < y1,
            None => true,
        }
    }
    #[inline]
    fn blend(&mut self, x: i32, y: i32, c: Color) {
        if x < 0 || y < 0 || x >= self.w as i32 || y >= self.h as i32 || c.a == 0 {
            return;
        }
        if !self.in_clip(x, y) {
            return;
        }
        let i = ((y as usize) * (self.w as usize) + (x as usize)) * 4;
        let d = &mut self.px[i..i + 4];
        if c.a == 255 {
            d[0] = c.r;
            d[1] = c.g;
            d[2] = c.b;
            d[3] = 255;
            return;
        }
        let sa = c.a as u32;
        let ia = 255 - sa;
        d[0] = ((c.r as u32 * sa + d[0] as u32 * ia) / 255) as u8;
        d[1] = ((c.g as u32 * sa + d[1] as u32 * ia) / 255) as u8;
        d[2] = ((c.b as u32 * sa + d[2] as u32 * ia) / 255) as u8;
        d[3] = (sa + d[3] as u32 * ia / 255).min(255) as u8;
    }
    /// Erase (set transparent) one pixel — used by clearRect.
    #[inline]
    fn erase(&mut self, x: i32, y: i32) {
        if x < 0 || y < 0 || x >= self.w as i32 || y >= self.h as i32 {
            return;
        }
        if !self.in_clip(x, y) {
            return;
        }
        let i = ((y as usize) * (self.w as usize) + (x as usize)) * 4;
        self.px[i..i + 4].fill(0);
    }
}

/// Read a command's optional `clip` field into a device-pixel rect `(x0, y0, x1, y1)` (exclusive
/// upper bound). Returns `None` when absent (no clip).
fn read_clip(cmd: &Json) -> Option<(i32, i32, i32, i32)> {
    let c = cmd.get("clip")?.as_arr();
    if c.len() < 4 {
        return None;
    }
    let x = c[0].num() as f32;
    let y = c[1].num() as f32;
    let w = c[2].num() as f32;
    let h = c[3].num() as f32;
    Some((
        x.floor() as i32,
        y.floor() as i32,
        (x + w).ceil() as i32,
        (y + h).ceil() as i32,
    ))
}

// ----------------------------------------------------------------------------------------------
// Color resolution: a small CSS color parser (named/hex/rgb/rgba/hsl/hsla), plus gradients.
// ----------------------------------------------------------------------------------------------

/// A resolved paint source for a command: either a flat color or a gradient.
enum Paint {
    Flat(Color),
    Linear {
        x0: f32,
        y0: f32,
        x1: f32,
        y1: f32,
        stops: Vec<(f32, Color)>,
    },
    Radial {
        x0: f32,
        y0: f32,
        r0: f32,
        x1: f32,
        y1: f32,
        r1: f32,
        stops: Vec<(f32, Color)>,
    },
}

impl Paint {
    /// Resolve from a command object, applying `alpha` (globalAlpha) to the flat-color case and to
    /// each gradient stop.
    fn from_cmd(cmd: &Json, alpha: f32) -> Paint {
        if let Some(g) = cmd.get("gradient") {
            let stops: Vec<(f32, Color)> = cmd
                .get("stops")
                .map(|s| s.as_arr())
                .unwrap_or(&[])
                .iter()
                .filter_map(|st| {
                    let off = st.f("offset", 0.0) as f32;
                    parse_css_color(st.get("color").map(|c| c.as_str()).unwrap_or("#000"))
                        .map(|c| (off.clamp(0.0, 1.0), apply_alpha(c, alpha)))
                })
                .collect();
            return match g.as_str() {
                "radial" => Paint::Radial {
                    x0: cmd.f("x0", 0.0) as f32,
                    y0: cmd.f("y0", 0.0) as f32,
                    r0: cmd.f("r0", 0.0) as f32,
                    x1: cmd.f("x1", 0.0) as f32,
                    y1: cmd.f("y1", 0.0) as f32,
                    r1: cmd.f("r1", 0.0) as f32,
                    stops,
                },
                _ => Paint::Linear {
                    x0: cmd.f("x0", 0.0) as f32,
                    y0: cmd.f("y0", 0.0) as f32,
                    x1: cmd.f("x1", 0.0) as f32,
                    y1: cmd.f("y1", 0.0) as f32,
                    stops,
                },
            };
        }
        let c = parse_css_color(cmd.get("color").map(|c| c.as_str()).unwrap_or("#000")).unwrap_or(
            Color {
                r: 0,
                g: 0,
                b: 0,
                a: 255,
            },
        );
        Paint::Flat(apply_alpha(c, alpha))
    }
    /// Color at a device-space point.
    fn at(&self, px: f32, py: f32) -> Color {
        match self {
            Paint::Flat(c) => *c,
            Paint::Linear {
                x0,
                y0,
                x1,
                y1,
                stops,
            } => {
                let dx = x1 - x0;
                let dy = y1 - y0;
                let len2 = dx * dx + dy * dy;
                let t = if len2 <= 1e-6 {
                    0.0
                } else {
                    ((px - x0) * dx + (py - y0) * dy) / len2
                };
                sample_stops(stops, t)
            }
            Paint::Radial {
                x0,
                y0,
                r0,
                x1,
                y1,
                r1,
                stops,
            } => {
                // Approximate: parameterize by distance from the END circle's center between r0..r1.
                let d = ((px - x1).powi(2) + (py - y1).powi(2)).sqrt();
                let denom = r1 - r0;
                let t = if denom.abs() <= 1e-6 {
                    if d <= *r1 {
                        1.0
                    } else {
                        0.0
                    }
                } else {
                    (d - r0) / denom
                };
                let _ = (x0, y0);
                sample_stops(stops, t)
            }
        }
    }
}

fn sample_stops(stops: &[(f32, Color)], t: f32) -> Color {
    if stops.is_empty() {
        return Color {
            r: 0,
            g: 0,
            b: 0,
            a: 0,
        };
    }
    let t = t.clamp(0.0, 1.0);
    if t <= stops[0].0 {
        return stops[0].1;
    }
    if t >= stops[stops.len() - 1].0 {
        return stops[stops.len() - 1].1;
    }
    for w in stops.windows(2) {
        let (o0, c0) = w[0];
        let (o1, c1) = w[1];
        if t >= o0 && t <= o1 {
            let f = if (o1 - o0).abs() <= 1e-6 {
                0.0
            } else {
                (t - o0) / (o1 - o0)
            };
            return lerp_color(c0, c1, f);
        }
    }
    stops[stops.len() - 1].1
}

fn lerp_color(a: Color, b: Color, f: f32) -> Color {
    let l = |x: u8, y: u8| {
        (x as f32 + (y as f32 - x as f32) * f)
            .round()
            .clamp(0.0, 255.0) as u8
    };
    Color {
        r: l(a.r, b.r),
        g: l(a.g, b.g),
        b: l(a.b, b.b),
        a: l(a.a, b.a),
    }
}

fn apply_alpha(c: Color, alpha: f32) -> Color {
    Color {
        a: ((c.a as f32) * alpha.clamp(0.0, 1.0))
            .round()
            .clamp(0.0, 255.0) as u8,
        ..c
    }
}

/// Public re-export of the canvas CSS color parser so the SVG module can reuse it (named/hex/
/// rgb/hsl/transparent) instead of duplicating the table.
pub fn parse_css_color_pub(s: &str) -> Option<Color> {
    parse_css_color(s)
}

/// Parse a CSS color: `#rgb`/`#rgba`/`#rrggbb`/`#rrggbbaa`, `rgb()`/`rgba()`, `hsl()`/`hsla()`,
/// `transparent`, and a set of common named colors. Returns `None` if unrecognized.
fn parse_css_color(s: &str) -> Option<Color> {
    let s = s.trim();
    if s.eq_ignore_ascii_case("transparent") {
        return Some(Color {
            r: 0,
            g: 0,
            b: 0,
            a: 0,
        });
    }
    if let Some(hex) = s.strip_prefix('#') {
        return parse_hex(hex);
    }
    let lower = s.to_ascii_lowercase();
    if lower.starts_with("rgb") {
        return parse_rgb(&lower);
    }
    if lower.starts_with("hsl") {
        return parse_hsl(&lower);
    }
    named_color(&lower)
}

fn parse_hex(hex: &str) -> Option<Color> {
    let h = hex.trim();
    let bytes = h.as_bytes();
    let hx = |a: u8, b: u8| -> Option<u8> {
        let d = |c: u8| (c as char).to_digit(16);
        Some((d(a)? * 16 + d(b)?) as u8)
    };
    match h.len() {
        3 => Some(Color {
            r: hx(bytes[0], bytes[0])?,
            g: hx(bytes[1], bytes[1])?,
            b: hx(bytes[2], bytes[2])?,
            a: 255,
        }),
        4 => Some(Color {
            r: hx(bytes[0], bytes[0])?,
            g: hx(bytes[1], bytes[1])?,
            b: hx(bytes[2], bytes[2])?,
            a: hx(bytes[3], bytes[3])?,
        }),
        6 => Some(Color {
            r: hx(bytes[0], bytes[1])?,
            g: hx(bytes[2], bytes[3])?,
            b: hx(bytes[4], bytes[5])?,
            a: 255,
        }),
        8 => Some(Color {
            r: hx(bytes[0], bytes[1])?,
            g: hx(bytes[2], bytes[3])?,
            b: hx(bytes[4], bytes[5])?,
            a: hx(bytes[6], bytes[7])?,
        }),
        _ => None,
    }
}

fn parse_rgb(s: &str) -> Option<Color> {
    let inner = s.split_once('(')?.1.trim_end_matches(')');
    let parts: Vec<&str> = inner
        .split([',', '/', ' '])
        .filter(|p| !p.trim().is_empty())
        .collect();
    if parts.len() < 3 {
        return None;
    }
    let chan = |p: &str| -> u8 {
        let p = p.trim();
        if let Some(pct) = p.strip_suffix('%') {
            (pct.trim().parse::<f32>().unwrap_or(0.0) / 100.0 * 255.0)
                .round()
                .clamp(0.0, 255.0) as u8
        } else {
            p.parse::<f32>().unwrap_or(0.0).round().clamp(0.0, 255.0) as u8
        }
    };
    let a = if parts.len() >= 4 {
        let p = parts[3].trim();
        if let Some(pct) = p.strip_suffix('%') {
            (pct.trim().parse::<f32>().unwrap_or(100.0) / 100.0 * 255.0)
                .round()
                .clamp(0.0, 255.0) as u8
        } else {
            (p.parse::<f32>().unwrap_or(1.0) * 255.0)
                .round()
                .clamp(0.0, 255.0) as u8
        }
    } else {
        255
    };
    Some(Color {
        r: chan(parts[0]),
        g: chan(parts[1]),
        b: chan(parts[2]),
        a,
    })
}

fn parse_hsl(s: &str) -> Option<Color> {
    let inner = s.split_once('(')?.1.trim_end_matches(')');
    let parts: Vec<&str> = inner
        .split([',', '/', ' '])
        .filter(|p| !p.trim().is_empty())
        .collect();
    if parts.len() < 3 {
        return None;
    }
    let h = parts[0]
        .trim()
        .trim_end_matches("deg")
        .parse::<f32>()
        .unwrap_or(0.0);
    let sl = parts[1]
        .trim()
        .trim_end_matches('%')
        .parse::<f32>()
        .unwrap_or(0.0)
        / 100.0;
    let l = parts[2]
        .trim()
        .trim_end_matches('%')
        .parse::<f32>()
        .unwrap_or(0.0)
        / 100.0;
    let a = if parts.len() >= 4 {
        let p = parts[3].trim();
        if let Some(pct) = p.strip_suffix('%') {
            (pct.trim().parse::<f32>().unwrap_or(100.0) / 100.0 * 255.0)
                .round()
                .clamp(0.0, 255.0) as u8
        } else {
            (p.parse::<f32>().unwrap_or(1.0) * 255.0)
                .round()
                .clamp(0.0, 255.0) as u8
        }
    } else {
        255
    };
    let (r, g, b) = hsl_to_rgb(h, sl, l);
    Some(Color { r, g, b, a })
}

fn hsl_to_rgb(h: f32, s: f32, l: f32) -> (u8, u8, u8) {
    let h = ((h % 360.0) + 360.0) % 360.0 / 360.0;
    let q = if l < 0.5 {
        l * (1.0 + s)
    } else {
        l + s - l * s
    };
    let p = 2.0 * l - q;
    let hue = |mut t: f32| -> f32 {
        if t < 0.0 {
            t += 1.0;
        }
        if t > 1.0 {
            t -= 1.0;
        }
        if t < 1.0 / 6.0 {
            p + (q - p) * 6.0 * t
        } else if t < 0.5 {
            q
        } else if t < 2.0 / 3.0 {
            p + (q - p) * (2.0 / 3.0 - t) * 6.0
        } else {
            p
        }
    };
    let (r, g, b) = if s == 0.0 {
        (l, l, l)
    } else {
        (hue(h + 1.0 / 3.0), hue(h), hue(h - 1.0 / 3.0))
    };
    (
        (r * 255.0).round().clamp(0.0, 255.0) as u8,
        (g * 255.0).round().clamp(0.0, 255.0) as u8,
        (b * 255.0).round().clamp(0.0, 255.0) as u8,
    )
}

fn named_color(name: &str) -> Option<Color> {
    let rgb = |r, g, b| Some(Color { r, g, b, a: 255 });
    match name {
        "black" => rgb(0, 0, 0),
        "white" => rgb(255, 255, 255),
        "red" => rgb(255, 0, 0),
        "green" => rgb(0, 128, 0),
        "lime" => rgb(0, 255, 0),
        "blue" => rgb(0, 0, 255),
        "yellow" => rgb(255, 255, 0),
        "cyan" | "aqua" => rgb(0, 255, 255),
        "magenta" | "fuchsia" => rgb(255, 0, 255),
        "gray" | "grey" => rgb(128, 128, 128),
        "silver" => rgb(192, 192, 192),
        "maroon" => rgb(128, 0, 0),
        "olive" => rgb(128, 128, 0),
        "navy" => rgb(0, 0, 128),
        "teal" => rgb(0, 128, 128),
        "purple" => rgb(128, 0, 128),
        "orange" => rgb(255, 165, 0),
        "pink" => rgb(255, 192, 203),
        "brown" => rgb(165, 42, 42),
        "gold" => rgb(255, 215, 0),
        "indigo" => rgb(75, 0, 130),
        "violet" => rgb(238, 130, 238),
        "darkgray" | "darkgrey" => rgb(169, 169, 169),
        "lightgray" | "lightgrey" => rgb(211, 211, 211),
        "darkblue" => rgb(0, 0, 139),
        "darkgreen" => rgb(0, 100, 0),
        "darkred" => rgb(139, 0, 0),
        "skyblue" => rgb(135, 206, 235),
        "steelblue" => rgb(70, 130, 180),
        "tomato" => rgb(255, 99, 71),
        "coral" => rgb(255, 127, 80),
        "transparent" => Some(Color {
            r: 0,
            g: 0,
            b: 0,
            a: 0,
        }),
        _ => None,
    }
}

// ----------------------------------------------------------------------------------------------
// Rasterizer.
// ----------------------------------------------------------------------------------------------

/// Rasterize one canvas's display list into a straight-alpha RGBA [`RasterImage`] of
/// `width`×`height` pixels (the canvas's pixel buffer; the engine scales it to the box's CSS size).
///
/// `font` (an optional [`GlyphRasterizer`]) drives the `text` op; pass `None` to skip glyph
/// rendering. `sources` maps a `drawImage` source node id to its `(rgba, w, h)` pixels; pass an
/// empty map to skip `drawImage`-from-node blits.
pub fn rasterize_canvas(
    cv: &CanvasList,
    font: Option<&dyn GlyphRasterizer>,
    sources: &HashMap<usize, (&[u8], u32, u32)>,
) -> RasterImage {
    let mut cnv = Canvas::new(cv.width, cv.height);
    for cmd in &cv.commands {
        let op = cmd.get("op").map(|o| o.as_str()).unwrap_or("");
        let alpha = cmd.f("alpha", 1.0) as f32;
        // Each command may carry a clip rect (bounding box of the clip path); set it for this op.
        cnv.clip = read_clip(cmd);
        match op {
            "fillRect" => {
                let paint = Paint::from_cmd(cmd, alpha);
                if let Some(quad) = read_quad(cmd) {
                    fill_quad(&mut cnv, quad, &paint);
                }
            }
            "clearRect" => {
                if let Some(quad) = read_quad(cmd) {
                    erase_quad(&mut cnv, quad);
                }
            }
            "fill" => {
                let paint = Paint::from_cmd(cmd, alpha);
                let polys = read_polylines(cmd, "polygons");
                fill_polygons(&mut cnv, &polys, &paint);
            }
            "stroke" => {
                let paint = Paint::from_cmd(cmd, alpha);
                let width = cmd.f("width", 1.0) as f32;
                let dash = read_dash(cmd);
                let dash_off = cmd.f("dashOffset", 0.0) as f32;
                let polys = read_polylines(cmd, "polylines");
                for poly in &polys {
                    if dash.is_empty() {
                        stroke_polyline(&mut cnv, poly, width, &paint);
                    } else {
                        stroke_polyline_dashed(&mut cnv, poly, width, &paint, &dash, dash_off);
                    }
                }
            }
            "text" => {
                let paint = Paint::from_cmd(cmd, alpha);
                if let (Some(font), Json::Str(text)) =
                    (font, cmd.get("text").unwrap_or(&Json::Null))
                {
                    draw_text(&mut cnv, font, cmd, text, &paint);
                }
            }
            "drawImage" => {
                draw_image(&mut cnv, cmd, alpha, sources);
            }
            "putImageData" => {
                put_image_data(&mut cnv, cmd);
            }
            _ => {}
        }
    }
    RasterImage {
        rgba: cnv.px,
        w: cv.width,
        h: cv.height,
    }
}

/// Read a stroke command's `dash` field (device-px on/off lengths).
fn read_dash(cmd: &Json) -> Vec<f32> {
    cmd.get("dash")
        .map(|d| {
            d.as_arr()
                .iter()
                .map(|v| (v.num() as f32).max(0.0))
                .collect()
        })
        .unwrap_or_default()
}

/// Blit a `drawImage` source into the destination quad. The source is the decoded `<img>` /
/// previous-frame canvas pixels keyed by `src` node id. Samples the `(sx,sy,sw,sh)` sub-rect with
/// nearest-neighbor, mapping into the (axis-aligned bounding box of the) device-space dest quad and
/// honoring globalAlpha + the active clip. Rotation/skew is approximated by the dest quad's AABB.
fn draw_image(
    cnv: &mut Canvas,
    cmd: &Json,
    alpha: f32,
    sources: &HashMap<usize, (&[u8], u32, u32)>,
) {
    let src_id = cmd.f("src", -1.0);
    if src_id < 0.0 {
        return;
    }
    let (px, sw_img, sh_img) = match sources.get(&(src_id as usize)) {
        Some(v) => *v,
        None => return,
    };
    if sw_img == 0 || sh_img == 0 {
        return;
    }
    let quad = match read_quad(cmd) {
        Some(q) => q,
        None => return,
    };
    // Source sub-rect (defaults to the whole image).
    let sx = cmd.f("sx", 0.0) as f32;
    let sy = cmd.f("sy", 0.0) as f32;
    let sw = cmd.f("sw", sw_img as f64) as f32;
    let sh = cmd.f("sh", sh_img as f64) as f32;
    if sw <= 0.0 || sh <= 0.0 {
        return;
    }
    // Dest = bounding box of the (transformed) dest quad.
    let (dx0, dx1, dy0, dy1) = poly_bounds(&quad, cnv.w, cnv.h);
    if dx1 <= dx0 || dy1 <= dy0 {
        return;
    }
    let dw = (dx1 - dx0) as f32;
    let dh = (dy1 - dy0) as f32;
    let a = alpha.clamp(0.0, 1.0);
    for y in dy0..dy1 {
        // Fractional vertical position within the dest box → source row.
        let fy = (y - dy0) as f32 + 0.5;
        let srcy = sy + (fy / dh) * sh;
        let iy = srcy.floor() as i32;
        if iy < 0 || iy >= sh_img as i32 {
            continue;
        }
        for x in dx0..dx1 {
            let fx = (x - dx0) as f32 + 0.5;
            let srcx = sx + (fx / dw) * sw;
            let ix = srcx.floor() as i32;
            if ix < 0 || ix >= sw_img as i32 {
                continue;
            }
            let si = ((iy as usize) * (sw_img as usize) + (ix as usize)) * 4;
            let sa = px[si + 3] as f32 * a;
            if sa <= 0.0 {
                continue;
            }
            let c = Color {
                r: px[si],
                g: px[si + 1],
                b: px[si + 2],
                a: sa.round().clamp(0.0, 255.0) as u8,
            };
            cnv.blend(x, y, c);
        }
    }
}

/// Write a `putImageData` pixel block into the canvas surface at `(dx,dy)` device pixels. The block
/// is base64 RGBA (`iw*ih*4` bytes). putImageData REPLACES pixels (it ignores alpha compositing and
/// the transform/clip per spec, but we still honor the canvas bounds). A `dirty*` sub-rect, if
/// present, restricts the written region.
fn put_image_data(cnv: &mut Canvas, cmd: &Json) {
    let dx = cmd.f("dx", 0.0) as i32;
    let dy = cmd.f("dy", 0.0) as i32;
    let iw = cmd.f("iw", 0.0) as i32;
    let ih = cmd.f("ih", 0.0) as i32;
    if iw <= 0 || ih <= 0 {
        return;
    }
    let b64 = cmd.get("b64").map(|v| v.as_str()).unwrap_or("");
    let data = base64_decode(b64);
    if data.len() < (iw as usize) * (ih as usize) * 4 {
        return;
    }
    // Dirty sub-rect (defaults to the whole block), clamped to the block.
    let (drx, dry, drw, drh) = if cmd.get("dirtyW").is_some() {
        (
            cmd.f("dirtyX", 0.0) as i32,
            cmd.f("dirtyY", 0.0) as i32,
            cmd.f("dirtyW", iw as f64) as i32,
            cmd.f("dirtyH", ih as f64) as i32,
        )
    } else {
        (0, 0, iw, ih)
    };
    let rx0 = drx.max(0);
    let ry0 = dry.max(0);
    let rx1 = (drx + drw).min(iw);
    let ry1 = (dry + drh).min(ih);
    for sy in ry0..ry1 {
        let ty = dy + sy;
        if ty < 0 || ty >= cnv.h as i32 {
            continue;
        }
        for sx in rx0..rx1 {
            let tx = dx + sx;
            if tx < 0 || tx >= cnv.w as i32 {
                continue;
            }
            let si = ((sy as usize) * (iw as usize) + (sx as usize)) * 4;
            let di = ((ty as usize) * (cnv.w as usize) + (tx as usize)) * 4;
            cnv.px[di..di + 4].copy_from_slice(&data[si..si + 4]);
        }
    }
}

/// Minimal standard-base64 encoder (no deps). Counterpart to [`base64_decode`]; used by tests.
#[cfg(test)]
fn base64_encode(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(T[(n >> 18 & 63) as usize] as char);
        out.push(T[(n >> 12 & 63) as usize] as char);
        out.push(if chunk.len() > 1 {
            T[(n >> 6 & 63) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            T[(n & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}

/// Minimal standard-base64 decoder (no deps), for `putImageData` pixel blocks. Ignores whitespace;
/// stops at padding. Returns the decoded bytes.
fn base64_decode(s: &str) -> Vec<u8> {
    fn val(c: u8) -> Option<u8> {
        match c {
            b'A'..=b'Z' => Some(c - b'A'),
            b'a'..=b'z' => Some(c - b'a' + 26),
            b'0'..=b'9' => Some(c - b'0' + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }
    let mut out = Vec::with_capacity(s.len() / 4 * 3);
    let mut acc = 0u32;
    let mut bits = 0u32;
    for &c in s.as_bytes() {
        let v = match val(c) {
            Some(v) => v,
            None => continue, // skip '=' / whitespace / newlines
        };
        acc = (acc << 6) | v as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    out
}

/// Draw a polyline as a dashed thick stroke: walk the path by arc length, alternating on/off
/// segments per the `dash` pattern (offset by `dash_offset`). On-segments are stroked as the normal
/// thick polyline; off-segments are skipped.
fn stroke_polyline_dashed(
    cnv: &mut Canvas,
    pts: &[(f32, f32)],
    width: f32,
    paint: &Paint,
    dash: &[f32],
    dash_offset: f32,
) {
    let total: f32 = dash.iter().sum();
    if total <= 1e-3 {
        stroke_polyline(cnv, pts, width, paint);
        return;
    }
    // Fast-forward to where the (wrapped) dash offset lands: `seg` = current pattern index, `rem` =
    // remaining length in that segment.
    let mut d = dash_offset.rem_euclid(total);
    let mut seg = 0usize;
    let mut rem;
    loop {
        if d < dash[seg] {
            rem = dash[seg] - d;
            break;
        }
        d -= dash[seg];
        seg = (seg + 1) % dash.len();
    }
    let mut on = seg.is_multiple_of(2); // even indices are "on" dashes
    for w in pts.windows(2) {
        let (ax, ay) = w[0];
        let (bx, by) = w[1];
        let dx = bx - ax;
        let dy = by - ay;
        let len = (dx * dx + dy * dy).sqrt();
        if len <= 1e-4 {
            continue;
        }
        let ux = dx / len;
        let uy = dy / len;
        let mut pos = 0.0f32; // distance walked along this segment
        while pos < len {
            let step = rem.min(len - pos);
            if on && step > 0.0 {
                let x0 = ax + ux * pos;
                let y0 = ay + uy * pos;
                let x1 = ax + ux * (pos + step);
                let y1 = ay + uy * (pos + step);
                stroke_polyline(cnv, &[(x0, y0), (x1, y1)], width, paint);
            }
            pos += step;
            rem -= step;
            if rem <= 1e-4 {
                seg = (seg + 1) % dash.len();
                rem = dash[seg];
                on = !on;
            }
        }
    }
}

/// Read a `quad` field (8 numbers: 4 corners) into device-space points.
fn read_quad(cmd: &Json) -> Option<[(f32, f32); 4]> {
    let q = cmd.get("quad")?.as_arr();
    if q.len() < 8 {
        return None;
    }
    Some([
        (q[0].num() as f32, q[1].num() as f32),
        (q[2].num() as f32, q[3].num() as f32),
        (q[4].num() as f32, q[5].num() as f32),
        (q[6].num() as f32, q[7].num() as f32),
    ])
}

/// Read an array-of-polylines field (each polyline is a flat `[x0,y0,x1,y1,...]` number array).
fn read_polylines(cmd: &Json, key: &str) -> Vec<Vec<(f32, f32)>> {
    let mut out = Vec::new();
    if let Some(arr) = cmd.get(key) {
        for poly in arr.as_arr() {
            let nums = poly.as_arr();
            let mut pts = Vec::with_capacity(nums.len() / 2);
            let mut k = 0;
            while k + 1 < nums.len() {
                pts.push((nums[k].num() as f32, nums[k + 1].num() as f32));
                k += 2;
            }
            if pts.len() >= 2 {
                out.push(pts);
            }
        }
    }
    out
}

/// Scanline fill of a convex (or simple) quad with a paint source (flat or gradient).
fn fill_quad(cnv: &mut Canvas, pts: [(f32, f32); 4], paint: &Paint) {
    fill_polygon_pts(cnv, &pts, paint);
}

fn erase_quad(cnv: &mut Canvas, pts: [(f32, f32); 4]) {
    let (minx, maxx, miny, maxy) = poly_bounds(&pts, cnv.w, cnv.h);
    for y in miny..maxy {
        let py = y as f32 + 0.5;
        for x in minx..maxx {
            let px = x as f32 + 0.5;
            if point_in_poly(&pts, px, py) {
                cnv.erase(x, y);
            }
        }
    }
}

/// Even-odd scanline fill of one or more (closed) polygons.
fn fill_polygons(cnv: &mut Canvas, polys: &[Vec<(f32, f32)>], paint: &Paint) {
    if polys.is_empty() {
        return;
    }
    // Combined bounds.
    let mut minx = f32::MAX;
    let mut maxx = f32::MIN;
    let mut miny = f32::MAX;
    let mut maxy = f32::MIN;
    for p in polys {
        for &(x, y) in p {
            minx = minx.min(x);
            maxx = maxx.max(x);
            miny = miny.min(y);
            maxy = maxy.max(y);
        }
    }
    let x0 = minx.floor().max(0.0) as i32;
    let x1 = (maxx.ceil() as i32).min(cnv.w as i32);
    let y0 = miny.floor().max(0.0) as i32;
    let y1 = (maxy.ceil() as i32).min(cnv.h as i32);
    if x1 <= x0 || y1 <= y0 {
        return;
    }
    let mut xs: Vec<f32> = Vec::new();
    for y in y0..y1 {
        let py = y as f32 + 0.5;
        xs.clear();
        for poly in polys {
            let n = poly.len();
            for i in 0..n {
                let (ax, ay) = poly[i];
                let (bx, by) = poly[(i + 1) % n];
                // Does the scanline cross this edge?
                if (ay <= py && by > py) || (by <= py && ay > py) {
                    let t = (py - ay) / (by - ay);
                    xs.push(ax + t * (bx - ax));
                }
            }
        }
        xs.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let mut i = 0;
        while i + 1 < xs.len() {
            let sx = xs[i].max(x0 as f32);
            let ex = xs[i + 1].min(x1 as f32);
            let sxi = sx.round() as i32;
            let exi = ex.round() as i32;
            for x in sxi..exi {
                let c = paint.at(x as f32 + 0.5, py);
                cnv.blend(x, y, c);
            }
            i += 2;
        }
    }
}

/// Fill a small fixed-size polygon (e.g. a quad) via the generic polygon filler.
fn fill_polygon_pts(cnv: &mut Canvas, pts: &[(f32, f32)], paint: &Paint) {
    fill_polygons(cnv, std::slice::from_ref(&pts.to_vec()), paint);
}

fn poly_bounds(pts: &[(f32, f32)], w: u32, h: u32) -> (i32, i32, i32, i32) {
    let mut minx = f32::MAX;
    let mut maxx = f32::MIN;
    let mut miny = f32::MAX;
    let mut maxy = f32::MIN;
    for &(x, y) in pts {
        minx = minx.min(x);
        maxx = maxx.max(x);
        miny = miny.min(y);
        maxy = maxy.max(y);
    }
    (
        minx.floor().max(0.0) as i32,
        (maxx.ceil() as i32).min(w as i32),
        miny.floor().max(0.0) as i32,
        (maxy.ceil() as i32).min(h as i32),
    )
}

fn point_in_poly(pts: &[(f32, f32)], px: f32, py: f32) -> bool {
    let mut inside = false;
    let n = pts.len();
    let mut j = n - 1;
    for i in 0..n {
        let (xi, yi) = pts[i];
        let (xj, yj) = pts[j];
        if (yi > py) != (yj > py) {
            let xint = (xj - xi) * (py - yi) / (yj - yi) + xi;
            if px < xint {
                inside = !inside;
            }
        }
        j = i;
    }
    inside
}

/// Draw a polyline as a series of thick segments, each a filled quad of width `width` centered on
/// the segment (square joins/caps — approximate).
fn stroke_polyline(cnv: &mut Canvas, pts: &[(f32, f32)], width: f32, paint: &Paint) {
    let hw = (width.max(0.1)) / 2.0;
    for seg in pts.windows(2) {
        let (ax, ay) = seg[0];
        let (bx, by) = seg[1];
        let dx = bx - ax;
        let dy = by - ay;
        let len = (dx * dx + dy * dy).sqrt();
        if len <= 1e-4 {
            // Degenerate: a dot of size width.
            let quad = [
                (ax - hw, ay - hw),
                (ax + hw, ay - hw),
                (ax + hw, ay + hw),
                (ax - hw, ay + hw),
            ];
            fill_polygon_pts(cnv, &quad, paint);
            continue;
        }
        // Unit normal.
        let nx = -dy / len * hw;
        let ny = dx / len * hw;
        let quad = [
            (ax + nx, ay + ny),
            (bx + nx, by + ny),
            (bx - nx, by - ny),
            (ax - nx, ay - ny),
        ];
        fill_polygon_pts(cnv, &quad, paint);
    }
}

/// Rasterize a text command's glyphs via the system font, honoring `align`. The command's `x`/`y`
/// is the (transformed) pen position; `size` is the device px size. Baseline is treated as
/// alphabetic (the y given), matching the common default.
fn draw_text(cnv: &mut Canvas, font: &dyn GlyphRasterizer, cmd: &Json, text: &str, paint: &Paint) {
    let mut x = cmd.f("x", 0.0) as f32;
    let y = cmd.f("y", 0.0) as f32;
    let size = (cmd.f("size", 10.0) as f32).max(1.0);
    let align = cmd.get("align").map(|a| a.as_str()).unwrap_or("start");
    // Measure with the real font for accurate alignment.
    let advance: f32 = text.chars().map(|ch| font.advance(ch, size)).sum();
    match align {
        "center" => x -= advance / 2.0,
        "right" | "end" => x -= advance,
        _ => {} // left / start
    }
    // Vertical baseline adjust for the common non-alphabetic values (approximate).
    let baseline = cmd
        .get("baseline")
        .map(|b| b.as_str())
        .unwrap_or("alphabetic");
    let y = match baseline {
        "top" | "hanging" => y + size * 0.8,
        "middle" => y + size * 0.3,
        "bottom" | "ideographic" => y - size * 0.15,
        _ => y, // alphabetic
    };
    let mut pen = x;
    for ch in text.chars() {
        if let Some(g) = font.rasterize(ch, size) {
            for row in 0..g.height {
                for col in 0..g.width {
                    let cov = g.coverage[row * g.width + col];
                    if cov == 0 {
                        continue;
                    }
                    let dx = pen as i32 + g.left + col as i32;
                    let dy = y as i32 + g.top + row as i32;
                    let c = paint.at(dx as f32 + 0.5, dy as f32 + 0.5);
                    let cc = Color {
                        a: ((c.a as u16 * cov as u16) / 255) as u8,
                        ..c
                    };
                    cnv.blend(dx, dy, cc);
                }
            }
            pen += g.advance;
        } else {
            pen += font.advance(ch, size);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_hex_and_named() {
        assert_eq!(
            parse_css_color("#ff0000"),
            Some(Color {
                r: 255,
                g: 0,
                b: 0,
                a: 255
            })
        );
        assert_eq!(
            parse_css_color("#f00"),
            Some(Color {
                r: 255,
                g: 0,
                b: 0,
                a: 255
            })
        );
        assert_eq!(
            parse_css_color("red"),
            Some(Color {
                r: 255,
                g: 0,
                b: 0,
                a: 255
            })
        );
        assert_eq!(
            parse_css_color("rgba(0,128,255,0.5)").map(|c| (c.r, c.g, c.b)),
            Some((0, 128, 255))
        );
    }

    #[test]
    fn parses_a_simple_list() {
        let json = r##"[{"id":3,"width":40,"height":20,"commands":[{"op":"fillRect","quad":[0,0,10,0,10,10,0,10],"color":"#ff0000","alpha":1}]}]"##;
        let lists = parse_canvas_lists(json);
        assert_eq!(lists.len(), 1);
        assert_eq!(lists[0].id, 3);
        assert_eq!(lists[0].width, 40);
        assert_eq!(lists[0].commands.len(), 1);
    }

    /// Pixel at (x,y) of a freshly-rasterized canvas.
    fn px(img: &RasterImage, x: u32, y: u32) -> (u8, u8, u8, u8) {
        let i = ((y * img.w + x) * 4) as usize;
        (
            img.rgba[i],
            img.rgba[i + 1],
            img.rgba[i + 2],
            img.rgba[i + 3],
        )
    }

    fn rasterize(json: &str, sources: &HashMap<usize, (&[u8], u32, u32)>) -> RasterImage {
        let lists = parse_canvas_lists(json);
        rasterize_canvas(&lists[0], None, sources)
    }

    #[test]
    fn base64_roundtrips() {
        for data in [
            &b"\x00\x01\x02\x03"[..],
            &b"hello world"[..],
            &b"\xff\xff"[..],
        ] {
            let enc = base64_encode(data);
            assert_eq!(base64_decode(&enc), data);
        }
    }

    #[test]
    fn draw_image_blits_source_pixels() {
        // A 2x2 source: solid red. drawImage it into a 4x4 canvas at the whole canvas dest rect.
        let red: Vec<u8> = std::iter::repeat_n([255u8, 0, 0, 255], 4)
            .flatten()
            .collect();
        let mut sources = HashMap::new();
        sources.insert(9usize, (red.as_slice(), 2u32, 2u32));
        let json = r##"[{"id":1,"width":4,"height":4,"commands":[
            {"op":"drawImage","src":9,"sx":0,"sy":0,"sw":2,"sh":2,"quad":[0,0,4,0,4,4,0,4],"alpha":1}]}]"##;
        let img = rasterize(json, &sources);
        // Every pixel should be red (the 2x2 source upscaled to 4x4).
        assert_eq!(px(&img, 0, 0), (255, 0, 0, 255));
        assert_eq!(px(&img, 3, 3), (255, 0, 0, 255));
    }

    #[test]
    fn draw_image_honors_global_alpha() {
        let red: Vec<u8> = std::iter::repeat_n([255u8, 0, 0, 255], 4)
            .flatten()
            .collect();
        let mut sources = HashMap::new();
        sources.insert(9usize, (red.as_slice(), 2u32, 2u32));
        let json = r##"[{"id":1,"width":4,"height":4,"commands":[
            {"op":"drawImage","src":9,"sx":0,"sy":0,"sw":2,"sh":2,"quad":[0,0,4,0,4,4,0,4],"alpha":0.5}]}]"##;
        let img = rasterize(json, &sources);
        let (_r, _, _, a) = px(&img, 1, 1);
        // Straight-alpha source-over onto a transparent canvas yields a half-alpha red pixel.
        assert!((120..=136).contains(&a), "alpha {a} ~= 128");
    }

    #[test]
    fn put_image_data_writes_block() {
        // A 2x2 red block (base64 of 16 bytes RGBA) put at (1,1) in a 4x4 canvas.
        let block: Vec<u8> = std::iter::repeat_n([255u8, 0, 0, 255], 4)
            .flatten()
            .collect();
        let b64 = base64_encode(&block);
        let json = format!(
            r##"[{{"id":1,"width":4,"height":4,"commands":[
            {{"op":"putImageData","dx":1,"dy":1,"iw":2,"ih":2,"b64":"{b64}"}}]}}]"##
        );
        let sources = HashMap::new();
        let img = rasterize(&json, &sources);
        assert_eq!(px(&img, 1, 1), (255, 0, 0, 255));
        assert_eq!(px(&img, 2, 2), (255, 0, 0, 255));
        // Outside the 2x2 block at (1,1) stays transparent.
        assert_eq!(px(&img, 0, 0), (0, 0, 0, 0));
        assert_eq!(px(&img, 3, 3), (0, 0, 0, 0));
    }

    #[test]
    fn clip_constrains_fill() {
        // Fill the whole 10x10 canvas red, but clip to the left 4px column.
        let json = r##"[{"id":1,"width":10,"height":10,"commands":[
            {"op":"fillRect","quad":[0,0,10,0,10,10,0,10],"color":"#ff0000","alpha":1,"clip":[0,0,4,10]}]}]"##;
        let sources = HashMap::new();
        let img = rasterize(json, &sources);
        assert_eq!(px(&img, 1, 5), (255, 0, 0, 255)); // inside clip → painted
        assert_eq!(px(&img, 8, 5), (0, 0, 0, 0)); // outside clip → untouched
    }

    #[test]
    fn dashed_stroke_has_gaps() {
        // A horizontal line y=5 from x=0..20, dash [4,4]: expect some on and some off pixels.
        let json = r##"[{"id":1,"width":20,"height":10,"commands":[
            {"op":"stroke","polylines":[[0,5,20,5]],"width":2,"color":"#000000","alpha":1,"dash":[4,4],"dashOffset":0}]}]"##;
        let sources = HashMap::new();
        let img = rasterize(json, &sources);
        let mut on = 0;
        let mut off = 0;
        for x in 0..20u32 {
            if px(&img, x, 5).3 > 0 {
                on += 1;
            } else {
                off += 1;
            }
        }
        assert!(on > 0, "dashed stroke painted no pixels");
        assert!(off > 0, "dashed stroke had no gaps");
    }
}
