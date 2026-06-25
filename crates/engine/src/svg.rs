//! Inline `<svg>` rasterizer: walks the parsed SVG DOM subtree directly (no JS round-trip — SVG
//! children are ordinary elements in the DOM snapshot) and rasterizes the common SVG primitives into
//! a straight-alpha RGBA bitmap that the engine composites exactly like a decoded `<img>` / canvas.
//!
//! Pipeline: **parse → flatten → rasterize**.
//!   * *Parse*: read presentation attributes (and inline `style=`) for paint state (fill/stroke/
//!     widths/opacities/fill-rule), plus the per-shape geometry attributes and `transform=`.
//!   * *Flatten*: every shape becomes one or more polylines in user space — curves/arcs/circles/
//!     ellipses are tessellated to short segments. The current affine transform (viewBox scale +
//!     ancestor `transform`s) maps user space → device pixels before rasterization.
//!   * *Rasterize*: reuse canvas-style primitives — even-odd / nonzero scanline polygon fill and a
//!     thick-quad polyline stroke — over a transparent RGBA surface.
//!
//! ## Supported
//! - Container: `<svg>` with `width`/`height` (px or unitless) + `viewBox="minx miny w h"`.
//!   viewBox → box mapping is a **uniform scale-to-fit** (min of x/y scale) centered (xMidYMid meet);
//!   `preserveAspectRatio` variants are not parsed (uniform fit only).
//! - Shapes: `<rect>` (x/y/width/height, rx/ry), `<circle>` (cx/cy/r), `<ellipse>` (cx/cy/rx/ry),
//!   `<line>`, `<polyline>`, `<polygon>`, `<path>`, `<text>` (x/y + text content).
//! - Path commands: M/m L/l H/h V/v C/c S/s Q/q T/t Z/z, **and A/a elliptical arcs** (flattened).
//! - Grouping: `<g>` applies its `transform` + inherited paint to children; paint inherits down the
//!   whole tree. `transform="translate|scale|rotate|matrix|skewX|skewY"` on any element.
//! - Paint: `fill` (color | `none`, default black), `stroke` (color | `none`), `stroke-width`
//!   (default 1), `fill-opacity` / `stroke-opacity` / `opacity`, `fill-rule` (nonzero | evenodd).
//!
//! ## Skipped (no-op, never crashes)
//! `<defs>` / gradients / `<use>` / `<symbol>` / `<clipPath>` / `<mask>` / filters / patterns,
//! `currentColor` (treated as black), CSS class/external-stylesheet styling, percentage lengths,
//! and `preserveAspectRatio` keywords other than the default uniform fit.

use dom::{Document, NodeData, NodeId};
use paint::{Color, GlyphRasterizer};

use crate::canvas::parse_css_color_pub as parse_css_color;
use crate::font::SystemFont;
use crate::DecodedImage;

const MAX_DIM: u32 = 4096;
/// Segments used to flatten a full circle/ellipse; fractions used for arcs/curves scale from this.
const CIRCLE_SEGMENTS: usize = 64;
const CURVE_SEGMENTS: usize = 24;

// ----------------------------------------------------------------------------------------------
// Affine transform (2x3): maps (x,y) -> (a*x + c*y + e, b*x + d*y + f).
// ----------------------------------------------------------------------------------------------

#[derive(Clone, Copy)]
struct Affine {
    a: f32,
    b: f32,
    c: f32,
    d: f32,
    e: f32,
    f: f32,
}

impl Affine {
    fn identity() -> Self {
        Affine {
            a: 1.0,
            b: 0.0,
            c: 0.0,
            d: 1.0,
            e: 0.0,
            f: 0.0,
        }
    }
    fn translate(tx: f32, ty: f32) -> Self {
        Affine {
            a: 1.0,
            b: 0.0,
            c: 0.0,
            d: 1.0,
            e: tx,
            f: ty,
        }
    }
    fn scale(sx: f32, sy: f32) -> Self {
        Affine {
            a: sx,
            b: 0.0,
            c: 0.0,
            d: sy,
            e: 0.0,
            f: 0.0,
        }
    }
    /// Matrix product `self ∘ other`: `other` is the *inner* (applied first) transform, `self` the
    /// outer. So `m.then(child)` composes the child transform inside `m` — a point is mapped by
    /// `child` then by `m`. Chaining a left-to-right SVG transform list as `m = m.then(t)` therefore
    /// makes the leftmost transform outermost (applied last), matching SVG semantics.
    fn then(self, o: Affine) -> Affine {
        Affine {
            a: self.a * o.a + self.c * o.b,
            b: self.b * o.a + self.d * o.b,
            c: self.a * o.c + self.c * o.d,
            d: self.b * o.c + self.d * o.d,
            e: self.a * o.e + self.c * o.f + self.e,
            f: self.b * o.e + self.d * o.f + self.f,
        }
    }
    fn apply(&self, x: f32, y: f32) -> (f32, f32) {
        (
            self.a * x + self.c * y + self.e,
            self.b * x + self.d * y + self.f,
        )
    }
    /// Approximate uniform scale factor (for mapping stroke-width user→device).
    fn mean_scale(&self) -> f32 {
        let sx = (self.a * self.a + self.b * self.b).sqrt();
        let sy = (self.c * self.c + self.d * self.d).sqrt();
        ((sx + sy) * 0.5).max(1e-4)
    }
}

// ----------------------------------------------------------------------------------------------
// Inherited paint state (cloned + overridden per element).
// ----------------------------------------------------------------------------------------------

#[derive(Clone)]
struct PaintState {
    fill: Option<Color>,   // None => fill:none
    stroke: Option<Color>, // None => stroke:none
    /// `fill="url(#id)"` reference (gradient). Takes precedence over `fill` when the id resolves to
    /// a gradient; otherwise `fill` is the solid fallback.
    fill_url: Option<String>,
    stroke_width: f32,
    fill_opacity: f32,
    stroke_opacity: f32,
    /// Group/element `opacity` accumulates multiplicatively down the tree.
    opacity: f32,
    evenodd: bool,
    /// In forced colors mode (and not forced-color-adjust:none), the element's forced `currentColor`
    /// — gradient stops authored with non-system colors resolve to it (so the gradient goes solid).
    force_stops: Option<(u8, u8, u8)>,
}

impl Default for PaintState {
    fn default() -> Self {
        PaintState {
            fill: Some(Color {
                r: 0,
                g: 0,
                b: 0,
                a: 255,
            }),
            stroke: None,
            fill_url: None,
            stroke_width: 1.0,
            fill_opacity: 1.0,
            stroke_opacity: 1.0,
            opacity: 1.0,
            evenodd: false,
            force_stops: None,
        }
    }
}

impl PaintState {
    /// The effective fill color, folding in fill-opacity and the accumulated element opacity.
    fn fill_color(&self) -> Option<Color> {
        self.fill
            .map(|c| apply_alpha(c, self.fill_opacity * self.opacity))
    }
    fn stroke_color(&self) -> Option<Color> {
        self.stroke
            .map(|c| apply_alpha(c, self.stroke_opacity * self.opacity))
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

// ----------------------------------------------------------------------------------------------
// RGBA surface (transparent; straight-alpha source-over) + scanline fill + thick stroke.
// ----------------------------------------------------------------------------------------------

struct Surface {
    w: u32,
    h: u32,
    px: Vec<u8>,
}

impl Surface {
    fn new(w: u32, h: u32) -> Self {
        Surface {
            w,
            h,
            px: vec![0u8; (w as usize) * (h as usize) * 4],
        }
    }
    #[inline]
    fn blend(&mut self, x: i32, y: i32, c: Color) {
        if x < 0 || y < 0 || x >= self.w as i32 || y >= self.h as i32 || c.a == 0 {
            return;
        }
        let i = ((y as usize) * (self.w as usize) + (x as usize)) * 4;
        let d = &mut self.px[i..i + 4];
        if c.a == 255 || d[3] == 0 {
            // Opaque source, OR painting onto a still-transparent pixel: store the source color
            // straight (its own alpha). Avoids the straight-alpha darkening that source-over toward
            // a transparent-black backdrop would otherwise cause on the first paint.
            d[0] = c.r;
            d[1] = c.g;
            d[2] = c.b;
            d[3] = c.a;
            return;
        }
        let sa = c.a as u32;
        let ia = 255 - sa;
        d[0] = ((c.r as u32 * sa + d[0] as u32 * ia) / 255) as u8;
        d[1] = ((c.g as u32 * sa + d[1] as u32 * ia) / 255) as u8;
        d[2] = ((c.b as u32 * sa + d[2] as u32 * ia) / 255) as u8;
        d[3] = (sa + d[3] as u32 * ia / 255).min(255) as u8;
    }
}

/// Scanline-fill one or more (closed) subpaths in device space with `evenodd`/nonzero winding.
fn fill_subpaths(surf: &mut Surface, subpaths: &[Vec<(f32, f32)>], color: Color, evenodd: bool) {
    if color.a == 0 {
        return;
    }
    let mut miny = f32::MAX;
    let mut maxy = f32::MIN;
    for p in subpaths {
        for &(_, y) in p {
            miny = miny.min(y);
            maxy = maxy.max(y);
        }
    }
    if !miny.is_finite() || !maxy.is_finite() {
        return;
    }
    let y0 = miny.floor().max(0.0) as i32;
    let y1 = (maxy.ceil() as i32).min(surf.h as i32);
    // Crossing: (x, winding direction +1/-1).
    let mut xs: Vec<(f32, i32)> = Vec::new();
    for y in y0..y1 {
        let py = y as f32 + 0.5;
        xs.clear();
        for poly in subpaths {
            let n = poly.len();
            if n < 2 {
                continue;
            }
            for i in 0..n {
                let (ax, ay) = poly[i];
                let (bx, by) = poly[(i + 1) % n];
                if (ay <= py && by > py) || (by <= py && ay > py) {
                    let t = (py - ay) / (by - ay);
                    let dir = if by > ay { 1 } else { -1 };
                    xs.push((ax + t * (bx - ax), dir));
                }
            }
        }
        xs.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
        if evenodd {
            let mut i = 0;
            while i + 1 < xs.len() {
                span(surf, xs[i].0, xs[i + 1].0, y, color);
                i += 2;
            }
        } else {
            // Nonzero: accumulate winding; a span is inside while the running count != 0.
            let mut wind = 0;
            let mut i = 0;
            while i < xs.len() {
                let prev = wind;
                wind += xs[i].1;
                if prev == 0 && wind != 0 {
                    // span starts at xs[i].0; find where winding returns to 0.
                    let start = xs[i].0;
                    let mut j = i + 1;
                    let mut w2 = wind;
                    while j < xs.len() && w2 != 0 {
                        w2 += xs[j].1;
                        j += 1;
                    }
                    if j >= 1 {
                        span(surf, start, xs[j - 1].0, y, color);
                    }
                    wind = w2;
                    i = j;
                    continue;
                }
                i += 1;
            }
        }
    }
}

#[inline]
fn span(surf: &mut Surface, sx: f32, ex: f32, y: i32, color: Color) {
    let sxi = sx.round().max(0.0) as i32;
    let exi = (ex.round() as i32).min(surf.w as i32);
    for x in sxi..exi {
        surf.blend(x, y, color);
    }
}

/// Stroke a polyline as a chain of thick quads (square caps/joins — approximate).
fn stroke_polyline(surf: &mut Surface, pts: &[(f32, f32)], width: f32, color: Color, closed: bool) {
    if color.a == 0 || pts.len() < 2 {
        return;
    }
    let hw = (width.max(0.1)) / 2.0;
    let n = pts.len();
    let segs = if closed { n } else { n - 1 };
    for k in 0..segs {
        let (ax, ay) = pts[k];
        let (bx, by) = pts[(k + 1) % n];
        let dx = bx - ax;
        let dy = by - ay;
        let len = (dx * dx + dy * dy).sqrt();
        let quad = if len <= 1e-4 {
            [
                (ax - hw, ay - hw),
                (ax + hw, ay - hw),
                (ax + hw, ay + hw),
                (ax - hw, ay + hw),
            ]
        } else {
            let nx = -dy / len * hw;
            let ny = dx / len * hw;
            [
                (ax + nx, ay + ny),
                (bx + nx, by + ny),
                (bx - nx, by - ny),
                (ax - nx, ay - ny),
            ]
        };
        fill_subpaths(surf, std::slice::from_ref(&quad.to_vec()), color, false);
    }
}

// ----------------------------------------------------------------------------------------------
// Transform attribute parser: translate / scale / rotate / matrix / skewX / skewY (chained L→R).
// ----------------------------------------------------------------------------------------------

fn parse_transform(s: &str) -> Affine {
    let mut m = Affine::identity();
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        // read function name
        while i < bytes.len() && !bytes[i].is_ascii_alphabetic() {
            i += 1;
        }
        let name_start = i;
        while i < bytes.len() && (bytes[i].is_ascii_alphabetic()) {
            i += 1;
        }
        if name_start == i {
            break;
        }
        let name = &s[name_start..i];
        // read '(' ... ')'
        while i < bytes.len() && bytes[i] != b'(' {
            i += 1;
        }
        if i >= bytes.len() {
            break;
        }
        i += 1; // (
        let args_start = i;
        while i < bytes.len() && bytes[i] != b')' {
            i += 1;
        }
        let args = &s[args_start..i.min(s.len())];
        i += 1; // )
        let nums = parse_numbers(args);
        let t = match name {
            "translate" => {
                let tx = nums.first().copied().unwrap_or(0.0);
                let ty = nums.get(1).copied().unwrap_or(0.0);
                Affine::translate(tx, ty)
            }
            "scale" => {
                let sx = nums.first().copied().unwrap_or(1.0);
                let sy = nums.get(1).copied().unwrap_or(sx);
                Affine::scale(sx, sy)
            }
            "rotate" => {
                let deg = nums.first().copied().unwrap_or(0.0);
                let (sin, cos) = deg.to_radians().sin_cos();
                let rot = Affine {
                    a: cos,
                    b: sin,
                    c: -sin,
                    d: cos,
                    e: 0.0,
                    f: 0.0,
                };
                if nums.len() >= 3 {
                    // rotate(angle, cx, cy) = translate(cx,cy) rotate translate(-cx,-cy)
                    let (cx, cy) = (nums[1], nums[2]);
                    Affine::translate(cx, cy)
                        .then(rot)
                        .then(Affine::translate(-cx, -cy))
                } else {
                    rot
                }
            }
            "matrix" => {
                if nums.len() >= 6 {
                    Affine {
                        a: nums[0],
                        b: nums[1],
                        c: nums[2],
                        d: nums[3],
                        e: nums[4],
                        f: nums[5],
                    }
                } else {
                    Affine::identity()
                }
            }
            "skewx" | "skewX" => {
                let t = nums.first().copied().unwrap_or(0.0).to_radians().tan();
                Affine {
                    a: 1.0,
                    b: 0.0,
                    c: t,
                    d: 1.0,
                    e: 0.0,
                    f: 0.0,
                }
            }
            "skewy" | "skewY" => {
                let t = nums.first().copied().unwrap_or(0.0).to_radians().tan();
                Affine {
                    a: 1.0,
                    b: t,
                    c: 0.0,
                    d: 1.0,
                    e: 0.0,
                    f: 0.0,
                }
            }
            _ => Affine::identity(),
        };
        m = m.then(t);
    }
    m
}

/// Parse a whitespace/comma separated number list (used by transforms, points, viewBox).
fn parse_numbers(s: &str) -> Vec<f32> {
    let mut out = Vec::new();
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        // skip separators
        while i < bytes.len() && matches!(bytes[i], b' ' | b',' | b'\t' | b'\n' | b'\r') {
            i += 1;
        }
        let start = i;
        if i < bytes.len() && (bytes[i] == b'-' || bytes[i] == b'+') {
            i += 1;
        }
        let mut seen = false;
        while i < bytes.len() && (bytes[i].is_ascii_digit() || bytes[i] == b'.') {
            i += 1;
            seen = true;
        }
        // exponent
        if seen && i < bytes.len() && (bytes[i] == b'e' || bytes[i] == b'E') {
            i += 1;
            if i < bytes.len() && (bytes[i] == b'-' || bytes[i] == b'+') {
                i += 1;
            }
            while i < bytes.len() && bytes[i].is_ascii_digit() {
                i += 1;
            }
        }
        if seen {
            if let Ok(n) = s[start..i].parse::<f32>() {
                out.push(n);
            }
        } else if i == start {
            i += 1; // no progress; force advance past unknown byte
        }
    }
    out
}

// ----------------------------------------------------------------------------------------------
// Path `d` parser → list of subpaths (each a polyline of user-space points). Curves/arcs flattened.
// ----------------------------------------------------------------------------------------------

struct PathParser<'a> {
    b: &'a [u8],
    s: &'a str,
    i: usize,
}

impl<'a> PathParser<'a> {
    fn new(s: &'a str) -> Self {
        PathParser {
            b: s.as_bytes(),
            s,
            i: 0,
        }
    }
    fn skip_sep(&mut self) {
        while self.i < self.b.len() && matches!(self.b[self.i], b' ' | b',' | b'\t' | b'\n' | b'\r')
        {
            self.i += 1;
        }
    }
    fn num(&mut self) -> Option<f32> {
        self.skip_sep();
        let start = self.i;
        if self.i < self.b.len() && (self.b[self.i] == b'-' || self.b[self.i] == b'+') {
            self.i += 1;
        }
        // Consume digits and at most ONE decimal point: in SVG path data numbers may be packed
        // without separators, where a second `.` begins a new number (e.g. `.78.16` = 0.78, 0.16).
        let mut seen = false;
        let mut seen_dot = false;
        while self.i < self.b.len() {
            let c = self.b[self.i];
            if c.is_ascii_digit() {
                self.i += 1;
                seen = true;
            } else if c == b'.' && !seen_dot {
                seen_dot = true;
                self.i += 1;
                seen = true;
            } else {
                break;
            }
        }
        if seen && self.i < self.b.len() && (self.b[self.i] == b'e' || self.b[self.i] == b'E') {
            self.i += 1;
            if self.i < self.b.len() && (self.b[self.i] == b'-' || self.b[self.i] == b'+') {
                self.i += 1;
            }
            while self.i < self.b.len() && self.b[self.i].is_ascii_digit() {
                self.i += 1;
            }
        }
        if !seen {
            return None;
        }
        self.s[start..self.i].parse::<f32>().ok()
    }
    /// A flag is a single `0`/`1` (arc large/sweep flags can be written without separators).
    fn flag(&mut self) -> Option<f32> {
        self.skip_sep();
        if self.i < self.b.len() && (self.b[self.i] == b'0' || self.b[self.i] == b'1') {
            let v = (self.b[self.i] - b'0') as f32;
            self.i += 1;
            Some(v)
        } else {
            self.num()
        }
    }
    fn peek_cmd(&mut self) -> Option<u8> {
        self.skip_sep();
        if self.i < self.b.len() && self.b[self.i].is_ascii_alphabetic() {
            Some(self.b[self.i])
        } else {
            None
        }
    }
}

/// Flatten a path's `d` into device-space subpaths and a parallel `closed` flag list.
#[allow(unused_assignments)] // the macro resets `cur_closed` after the final (loop-exit) finish!()
fn flatten_path(d: &str, m: &Affine) -> (Vec<Vec<(f32, f32)>>, Vec<bool>) {
    let mut subpaths: Vec<Vec<(f32, f32)>> = Vec::new();
    let mut closed: Vec<bool> = Vec::new();
    let mut cur: Vec<(f32, f32)> = Vec::new();
    let mut cur_closed = false;
    // User-space cursor + subpath start + last control points (for S/T smoothing).
    let mut px = 0.0f32;
    let mut py = 0.0f32;
    let mut start_x = 0.0f32;
    let mut start_y = 0.0f32;
    let mut last_c: Option<(f32, f32)> = None; // last cubic 2nd control
    let mut last_q: Option<(f32, f32)> = None; // last quad control

    let push = |cur: &mut Vec<(f32, f32)>, m: &Affine, x: f32, y: f32| {
        cur.push(m.apply(x, y));
    };
    macro_rules! finish {
        () => {
            if cur.len() >= 1 {
                subpaths.push(std::mem::take(&mut cur));
                closed.push(cur_closed);
            } else {
                cur.clear();
            }
            cur_closed = false;
        };
    }

    let mut p = PathParser::new(d);
    let mut cmd = 0u8;
    loop {
        let next = p.peek_cmd();
        if let Some(c) = next {
            cmd = c;
            p.i += 1;
        } else if cmd == 0 || p.i >= p.b.len() {
            // No new command letter and either nothing parsed yet or input exhausted: stop. (This
            // also terminates after a trailing `Z`, whose branch consumes no coordinates.)
            break;
        }
        let rel = cmd.is_ascii_lowercase();
        match cmd.to_ascii_uppercase() {
            b'M' => {
                let x = match p.num() {
                    Some(v) => v,
                    None => break,
                };
                let y = p.num().unwrap_or(0.0);
                finish!();
                px = if rel { px + x } else { x };
                py = if rel { py + y } else { y };
                start_x = px;
                start_y = py;
                push(&mut cur, m, px, py);
                last_c = None;
                last_q = None;
                // Subsequent coordinate pairs after M are implicit L.
                cmd = if rel { b'l' } else { b'L' };
            }
            b'L' => {
                let x = match p.num() {
                    Some(v) => v,
                    None => break,
                };
                let y = p.num().unwrap_or(0.0);
                px = if rel { px + x } else { x };
                py = if rel { py + y } else { y };
                push(&mut cur, m, px, py);
                last_c = None;
                last_q = None;
            }
            b'H' => {
                let x = match p.num() {
                    Some(v) => v,
                    None => break,
                };
                px = if rel { px + x } else { x };
                push(&mut cur, m, px, py);
                last_c = None;
                last_q = None;
            }
            b'V' => {
                let y = match p.num() {
                    Some(v) => v,
                    None => break,
                };
                py = if rel { py + y } else { y };
                push(&mut cur, m, px, py);
                last_c = None;
                last_q = None;
            }
            b'C' | b'S' => {
                let (x1, y1) = if cmd.eq_ignore_ascii_case(&b'C') {
                    let x1 = match p.num() {
                        Some(v) => v,
                        None => break,
                    };
                    let y1 = p.num().unwrap_or(0.0);
                    let x1 = if rel { px + x1 } else { x1 };
                    let y1 = if rel { py + y1 } else { y1 };
                    (x1, y1)
                } else {
                    // S: reflect last cubic control about current point.
                    match last_c {
                        Some((cx, cy)) => (2.0 * px - cx, 2.0 * py - cy),
                        None => (px, py),
                    }
                };
                let x2 = match p.num() {
                    Some(v) => v,
                    None => break,
                };
                let y2 = p.num().unwrap_or(0.0);
                let ex = p.num().unwrap_or(0.0);
                let ey = p.num().unwrap_or(0.0);
                let x2 = if rel { px + x2 } else { x2 };
                let y2 = if rel { py + y2 } else { y2 };
                let ex = if rel { px + ex } else { ex };
                let ey = if rel { py + ey } else { ey };
                flatten_cubic(&mut cur, m, px, py, x1, y1, x2, y2, ex, ey);
                last_c = Some((x2, y2));
                last_q = None;
                px = ex;
                py = ey;
            }
            b'Q' | b'T' => {
                let (cx, cy) = if cmd.eq_ignore_ascii_case(&b'Q') {
                    let cx = match p.num() {
                        Some(v) => v,
                        None => break,
                    };
                    let cy = p.num().unwrap_or(0.0);
                    let cx = if rel { px + cx } else { cx };
                    let cy = if rel { py + cy } else { cy };
                    (cx, cy)
                } else {
                    match last_q {
                        Some((qx, qy)) => (2.0 * px - qx, 2.0 * py - qy),
                        None => (px, py),
                    }
                };
                let ex = match p.num() {
                    Some(v) => v,
                    None => break,
                };
                let ey = p.num().unwrap_or(0.0);
                let ex = if rel { px + ex } else { ex };
                let ey = if rel { py + ey } else { ey };
                flatten_quad(&mut cur, m, px, py, cx, cy, ex, ey);
                last_q = Some((cx, cy));
                last_c = None;
                px = ex;
                py = ey;
            }
            b'A' => {
                let rx = match p.num() {
                    Some(v) => v,
                    None => break,
                };
                let ry = p.num().unwrap_or(0.0);
                let xrot = p.num().unwrap_or(0.0);
                let large = p.flag().unwrap_or(0.0) != 0.0;
                let sweep = p.flag().unwrap_or(0.0) != 0.0;
                let ex = p.num().unwrap_or(0.0);
                let ey = p.num().unwrap_or(0.0);
                let ex = if rel { px + ex } else { ex };
                let ey = if rel { py + ey } else { ey };
                flatten_arc(&mut cur, m, px, py, rx, ry, xrot, large, sweep, ex, ey);
                last_c = None;
                last_q = None;
                px = ex;
                py = ey;
            }
            b'Z' => {
                cur_closed = true;
                px = start_x;
                py = start_y;
                finish!();
                last_c = None;
                last_q = None;
                // After Z, the next command (if a coordinate) restarts at start point.
                if p.peek_cmd().is_none() && p.i < p.b.len() {
                    // A bare coordinate after Z is unusual; let the loop's num-less branch break.
                }
            }
            _ => {
                // Unknown command: stop to avoid an infinite loop.
                break;
            }
        }
    }
    finish!();
    (subpaths, closed)
}

fn flatten_cubic(
    cur: &mut Vec<(f32, f32)>,
    m: &Affine,
    x0: f32,
    y0: f32,
    x1: f32,
    y1: f32,
    x2: f32,
    y2: f32,
    x3: f32,
    y3: f32,
) {
    for k in 1..=CURVE_SEGMENTS {
        let t = k as f32 / CURVE_SEGMENTS as f32;
        let mt = 1.0 - t;
        let x = mt * mt * mt * x0 + 3.0 * mt * mt * t * x1 + 3.0 * mt * t * t * x2 + t * t * t * x3;
        let y = mt * mt * mt * y0 + 3.0 * mt * mt * t * y1 + 3.0 * mt * t * t * y2 + t * t * t * y3;
        cur.push(m.apply(x, y));
    }
}

fn flatten_quad(
    cur: &mut Vec<(f32, f32)>,
    m: &Affine,
    x0: f32,
    y0: f32,
    cx: f32,
    cy: f32,
    x1: f32,
    y1: f32,
) {
    for k in 1..=CURVE_SEGMENTS {
        let t = k as f32 / CURVE_SEGMENTS as f32;
        let mt = 1.0 - t;
        let x = mt * mt * x0 + 2.0 * mt * t * cx + t * t * x1;
        let y = mt * mt * y0 + 2.0 * mt * t * cy + t * t * y1;
        cur.push(m.apply(x, y));
    }
}

/// Flatten an SVG elliptical arc (endpoint parameterization → center parameterization → segments).
#[allow(clippy::too_many_arguments)]
fn flatten_arc(
    cur: &mut Vec<(f32, f32)>,
    m: &Affine,
    x0: f32,
    y0: f32,
    mut rx: f32,
    mut ry: f32,
    xrot_deg: f32,
    large: bool,
    sweep: bool,
    x1: f32,
    y1: f32,
) {
    if (x0 - x1).abs() < 1e-6 && (y0 - y1).abs() < 1e-6 {
        return;
    }
    if rx.abs() < 1e-6 || ry.abs() < 1e-6 {
        cur.push(m.apply(x1, y1)); // degenerate => straight line
        return;
    }
    rx = rx.abs();
    ry = ry.abs();
    let phi = xrot_deg.to_radians();
    let (sin_p, cos_p) = phi.sin_cos();
    let dx2 = (x0 - x1) / 2.0;
    let dy2 = (y0 - y1) / 2.0;
    let x1p = cos_p * dx2 + sin_p * dy2;
    let y1p = -sin_p * dx2 + cos_p * dy2;
    // Correct out-of-range radii.
    let lambda = (x1p * x1p) / (rx * rx) + (y1p * y1p) / (ry * ry);
    if lambda > 1.0 {
        let s = lambda.sqrt();
        rx *= s;
        ry *= s;
    }
    let rx2 = rx * rx;
    let ry2 = ry * ry;
    let num = (rx2 * ry2 - rx2 * y1p * y1p - ry2 * x1p * x1p).max(0.0);
    let den = rx2 * y1p * y1p + ry2 * x1p * x1p;
    let mut coef = if den > 0.0 { (num / den).sqrt() } else { 0.0 };
    if large == sweep {
        coef = -coef;
    }
    let cxp = coef * rx * y1p / ry;
    let cyp = -coef * ry * x1p / rx;
    let cx = cos_p * cxp - sin_p * cyp + (x0 + x1) / 2.0;
    let cy = sin_p * cxp + cos_p * cyp + (y0 + y1) / 2.0;
    let ang = |ux: f32, uy: f32, vx: f32, vy: f32| -> f32 {
        let dot = ux * vx + uy * vy;
        let len = (ux * ux + uy * uy).sqrt() * (vx * vx + vy * vy).sqrt();
        let mut a = (dot / len).clamp(-1.0, 1.0).acos();
        if ux * vy - uy * vx < 0.0 {
            a = -a;
        }
        a
    };
    let theta1 = ang(1.0, 0.0, (x1p - cxp) / rx, (y1p - cyp) / ry);
    let mut dtheta = ang(
        (x1p - cxp) / rx,
        (y1p - cyp) / ry,
        (-x1p - cxp) / rx,
        (-y1p - cyp) / ry,
    );
    if !sweep && dtheta > 0.0 {
        dtheta -= std::f32::consts::TAU;
    } else if sweep && dtheta < 0.0 {
        dtheta += std::f32::consts::TAU;
    }
    let steps =
        ((dtheta.abs() / std::f32::consts::TAU * CIRCLE_SEGMENTS as f32).ceil() as usize).max(2);
    for k in 1..=steps {
        let t = theta1 + dtheta * (k as f32 / steps as f32);
        let (sin_t, cos_t) = t.sin_cos();
        let x = cos_p * rx * cos_t - sin_p * ry * sin_t + cx;
        let y = sin_p * rx * cos_t + cos_p * ry * sin_t + cy;
        cur.push(m.apply(x, y));
    }
}

// ----------------------------------------------------------------------------------------------
// Shape flatteners (user space). Each returns subpaths; rects/ellipses are closed.
// ----------------------------------------------------------------------------------------------

fn flatten_ellipse(cx: f32, cy: f32, rx: f32, ry: f32, m: &Affine) -> Vec<(f32, f32)> {
    let mut pts = Vec::with_capacity(CIRCLE_SEGMENTS);
    for k in 0..CIRCLE_SEGMENTS {
        let a = k as f32 / CIRCLE_SEGMENTS as f32 * std::f32::consts::TAU;
        let (s, c) = a.sin_cos();
        pts.push(m.apply(cx + rx * c, cy + ry * s));
    }
    pts
}

fn flatten_rect(x: f32, y: f32, w: f32, h: f32, rx: f32, ry: f32, m: &Affine) -> Vec<(f32, f32)> {
    if rx <= 0.0 && ry <= 0.0 {
        return vec![
            m.apply(x, y),
            m.apply(x + w, y),
            m.apply(x + w, y + h),
            m.apply(x, y + h),
        ];
    }
    let rx = rx.min(w / 2.0);
    let ry = ry.min(h / 2.0);
    let seg = 8;
    let mut pts = Vec::new();
    // corner centers and start angles, walking clockwise from top-left's right edge.
    let corners = [
        (x + w - rx, y + ry, -std::f32::consts::FRAC_PI_2), // top-right
        (x + w - rx, y + h - ry, 0.0),                      // bottom-right
        (x + rx, y + h - ry, std::f32::consts::FRAC_PI_2),  // bottom-left
        (x + rx, y + ry, std::f32::consts::PI),             // top-left
    ];
    for &(ccx, ccy, a0) in &corners {
        for k in 0..=seg {
            let a = a0 + (k as f32 / seg as f32) * std::f32::consts::FRAC_PI_2;
            let (s, c) = a.sin_cos();
            pts.push(m.apply(ccx + rx * c, ccy + ry * s));
        }
    }
    pts
}

// ----------------------------------------------------------------------------------------------
// Attribute helpers.
// ----------------------------------------------------------------------------------------------

fn attr<'a>(el: &'a dom::ElementData, name: &str) -> Option<&'a str> {
    el.attrs.get(name).map(|s| s.as_str())
}

/// Number attribute (strips a trailing `px`); default `d`.
fn num_attr(el: &dom::ElementData, name: &str, d: f32) -> f32 {
    attr(el, name)
        .map(|s| parse_len(s).unwrap_or(d))
        .unwrap_or(d)
}

fn parse_len(s: &str) -> Option<f32> {
    let s = s.trim();
    let s = s.strip_suffix("px").unwrap_or(s);
    s.trim().parse::<f32>().ok()
}

/// Look up a presentation property: inline `style="..."` wins over the attribute of the same name.
fn prop<'a>(el: &'a dom::ElementData, name: &str, style: &'a Option<String>) -> Option<String> {
    if let Some(st) = style {
        if let Some(v) = style_prop(st, name) {
            return Some(v);
        }
    }
    attr(el, name).map(|s| s.trim().to_string())
}

fn style_prop(style: &str, name: &str) -> Option<String> {
    for decl in style.split(';') {
        if let Some((k, v)) = decl.split_once(':') {
            if k.trim().eq_ignore_ascii_case(name) {
                return Some(v.trim().to_string());
            }
        }
    }
    None
}

/// Resolve a paint value (`fill`/`stroke`): `none` → None paint; otherwise parse the color.
fn resolve_paint(val: &str, inherited: Option<Color>, current: Option<Color>) -> Option<Color> {
    let v = val.trim();
    if v.eq_ignore_ascii_case("none") {
        return None;
    }
    // `currentColor` resolves to the element's `color` property (not the inherited fill/stroke).
    if v.eq_ignore_ascii_case("currentcolor") {
        return current.or(inherited);
    }
    if v.eq_ignore_ascii_case("inherit") {
        return inherited;
    }
    // url(#...) gradients/patterns are unsupported → fall back to a mid-gray so the shape is visible.
    if v.starts_with("url(") {
        return Some(Color {
            r: 128,
            g: 128,
            b: 128,
            a: 255,
        });
    }
    parse_css_color(v).or(inherited)
}

type StyleMap = std::collections::HashMap<NodeId, style::ComputedStyle>;

/// Apply this element's presentation attributes/style onto an inherited paint state. `cs` is the
/// element's cascaded style (CSS `fill`/`stroke` override presentation attributes; forced colors
/// maps a painted fill/stroke to the element's `currentColor`, which the cascade already forced).
fn apply_paint(
    el: &dom::ElementData,
    cs: Option<&style::ComputedStyle>,
    mut st: PaintState,
) -> PaintState {
    let style = attr(el, "style").map(|s| s.to_string());
    // SVG `currentColor` resolves to the element's CSS `color` property. Inside a visited link, the
    // computed LinkText is mapped to VisitedText for painting (matching the painter's text mapping).
    let cur_rgb = cs.map(|c| {
        if c.visited_link && c.color == (0, 0, 238) {
            (85, 26, 139) // VisitedText
        } else {
            c.color
        }
    });
    let current = cur_rgb.map(|(r, g, b)| Color { r, g, b, a: 255 });
    if let Some(v) = prop(el, "fill", &style) {
        if let Some(id) = paint_url_id(&v) {
            // A `url(#id)` paint (gradient/pattern): remember the ref; keep a gray solid fallback in
            // case it doesn't resolve to a gradient we support.
            st.fill_url = Some(id);
            st.fill = Some(Color {
                r: 128,
                g: 128,
                b: 128,
                a: 255,
            });
        } else {
            st.fill_url = None;
            st.fill = resolve_paint(&v, st.fill, current);
        }
    }
    if let Some(v) = prop(el, "stroke", &style) {
        st.stroke = resolve_paint(&v, st.stroke, current);
    }
    if let Some(v) = prop(el, "stroke-width", &style) {
        if let Some(w) = parse_len(&v) {
            st.stroke_width = w;
        }
    }
    if let Some(v) = prop(el, "fill-opacity", &style) {
        if let Ok(o) = v.trim().parse::<f32>() {
            st.fill_opacity = o.clamp(0.0, 1.0);
        }
    }
    if let Some(v) = prop(el, "stroke-opacity", &style) {
        if let Ok(o) = v.trim().parse::<f32>() {
            st.stroke_opacity = o.clamp(0.0, 1.0);
        }
    }
    if let Some(v) = prop(el, "opacity", &style) {
        if let Ok(o) = v.trim().parse::<f32>() {
            st.opacity *= o.clamp(0.0, 1.0);
        }
    }
    if let Some(v) = prop(el, "fill-rule", &style) {
        st.evenodd = v.trim().eq_ignore_ascii_case("evenodd");
    }
    // CSS `fill`/`stroke` (from author `<style>` rules / inline `style`, resolved by the cascade)
    // override the SVG presentation attributes above.
    if let Some(cs) = cs {
        let (cr, cg, cb) = cur_rgb.unwrap_or(cs.color);
        if let Some(extra) = &cs.extra_colors {
            if let Some(&(r, g, b)) = extra.get("fill") {
                st.fill = Some(Color { r, g, b, a: 255 });
                st.fill_url = None;
            }
            if let Some(&(r, g, b)) = extra.get("stroke") {
                st.stroke = Some(Color { r, g, b, a: 255 });
            }
        }
        // `fill/stroke: currentColor` follows the element's (possibly forced) color, not the value
        // frozen at cascade time.
        if cs.svg_fill_current {
            st.fill = Some(Color {
                r: cr,
                g: cg,
                b: cb,
                a: 255,
            });
            st.fill_url = None;
        }
        if cs.svg_stroke_current {
            st.stroke = Some(Color {
                r: cr,
                g: cg,
                b: cb,
                a: 255,
            });
        }
        // Forced colors: a painted fill/stroke resolves to the element's (already-forced)
        // currentColor, unless the element opted out with forced-color-adjust:none. A `url()` paint
        // (gradient/pattern) is left alone — its own stops carry the forced/system colors.
        if style::forced_colors_active() && !cs.forced_color_adjust_off {
            let cc = Color {
                r: cr,
                g: cg,
                b: cb,
                a: 255,
            };
            if st.fill.is_some() && st.fill_url.is_none() {
                st.fill = Some(cc);
            }
            if st.stroke.is_some() {
                st.stroke = Some(cc);
            }
            st.force_stops = Some((cr, cg, cb));
        } else {
            st.force_stops = None;
        }
    }
    st
}

// ----------------------------------------------------------------------------------------------
// Recursive renderer over the SVG DOM subtree.
// ----------------------------------------------------------------------------------------------

/// Compute an inline `<svg>`'s intrinsic pixel size from `width`/`height` attrs, else the `viewBox`
/// w/h, else the spec default 300×150. Returns `(w, h)` (both ≥ 1).
pub fn intrinsic_size(el: &dom::ElementData) -> (f32, f32) {
    let w = attr(el, "width").and_then(parse_len);
    let h = attr(el, "height").and_then(parse_len);
    if let (Some(w), Some(h)) = (w, h) {
        return (w.max(1.0), h.max(1.0));
    }
    if let Some(vb) = attr(el, "viewbox").map(parse_numbers) {
        if vb.len() == 4 && vb[2] > 0.0 && vb[3] > 0.0 {
            let vw = vb[2];
            let vh = vb[3];
            return (w.unwrap_or(vw).max(1.0), h.unwrap_or(vh).max(1.0));
        }
    }
    (w.unwrap_or(300.0).max(1.0), h.unwrap_or(150.0).max(1.0))
}

/// Rasterize an inline `<svg>` DOM subtree into an RGBA [`DecodedImage`] of `out_w`×`out_h` device
/// pixels (the box's content size). Walks the subtree directly; honors `viewBox`, transforms, and
/// inherited paint.
pub fn rasterize_svg(
    doc: &Document,
    svg_id: NodeId,
    out_w: u32,
    out_h: u32,
    font: Option<&SystemFont>,
    styles: Option<&StyleMap>,
) -> DecodedImage {
    let out_w = out_w.clamp(1, MAX_DIM);
    let out_h = out_h.clamp(1, MAX_DIM);
    let mut surf = Surface::new(out_w, out_h);

    let el = match &doc.get(svg_id).data {
        NodeData::Element(e) => e,
        _ => {
            return DecodedImage {
                rgba: surf.px,
                w: out_w,
                h: out_h,
            }
        }
    };

    // viewBox → device: uniform scale-to-fit (xMidYMid meet), centered.
    let base = viewbox_affine(el, out_w as f32, out_h as f32);

    let root_state = apply_paint(
        el,
        styles.and_then(|m| m.get(&svg_id)),
        PaintState::default(),
    );
    render_children(doc, svg_id, base, &root_state, &mut surf, font, styles, 0);

    DecodedImage {
        rgba: surf.px,
        w: out_w,
        h: out_h,
    }
}

/// The `viewBox` → viewport affine for an `<svg>`/`<symbol>` element: uniform scale-to-fit
/// (xMidYMid meet) into a `out_w`×`out_h` box, centered. Identity when there's no usable viewBox.
fn viewbox_affine(el: &dom::ElementData, out_w: f32, out_h: f32) -> Affine {
    if let Some(vb) = attr(el, "viewbox").map(parse_numbers) {
        if vb.len() == 4 && vb[2] > 0.0 && vb[3] > 0.0 {
            let (minx, miny, vw, vh) = (vb[0], vb[1], vb[2], vb[3]);
            let scale = (out_w / vw).min(out_h / vh);
            let tx = (out_w - vw * scale) / 2.0;
            let ty = (out_h - vh * scale) / 2.0;
            // translate(tx,ty) ∘ scale(s) ∘ translate(-minx,-miny)
            return Affine::translate(tx, ty)
                .then(Affine::scale(scale, scale))
                .then(Affine::translate(-minx, -miny));
        }
    }
    Affine::identity()
}

/// Find the first element in the document with `id="id"` (for `<use xlink:href="#id">` resolution).
fn find_by_id(doc: &Document, id: &str) -> Option<NodeId> {
    (0..doc.len()).map(NodeId).find(|&n| {
        matches!(&doc.get(n).data,
            NodeData::Element(e) if e.attrs.get("id").map(|s| s == id).unwrap_or(false))
    })
}

/// Recurse over an element's children, rendering each shape / group. `depth` guards runaway nesting.
fn render_children(
    doc: &Document,
    parent: NodeId,
    m: Affine,
    state: &PaintState,
    surf: &mut Surface,
    font: Option<&SystemFont>,
    styles: Option<&StyleMap>,
    depth: usize,
) {
    if depth > 256 {
        return;
    }
    for &child in &doc.get(parent).children {
        render_element(doc, child, m, state, surf, font, styles, depth);
    }
}

/// Render a single SVG element (and its subtree) at the current transform `m` + inherited paint.
fn render_element(
    doc: &Document,
    child: NodeId,
    m: Affine,
    state: &PaintState,
    surf: &mut Surface,
    font: Option<&SystemFont>,
    styles: Option<&StyleMap>,
    depth: usize,
) {
    if depth > 256 {
        return;
    }
    {
        let el = match &doc.get(child).data {
            NodeData::Element(e) => e,
            _ => return,
        };
        let tag = el.tag.to_ascii_lowercase();
        // Skip non-rendered defs/metadata/etc. (gradients/clipPath are resolved on demand, not drawn
        // directly here). `<use>`/`<svg>`/`<symbol>` ARE handled below.
        if matches!(
            tag.as_str(),
            "defs"
                | "clippath"
                | "mask"
                | "lineargradient"
                | "radialgradient"
                | "pattern"
                | "filter"
                | "metadata"
                | "title"
                | "desc"
                | "style"
        ) {
            return;
        }
        let child_m = match attr(el, "transform") {
            Some(t) => m.then(parse_transform(t)),
            None => m,
        };
        let child_state = apply_paint(el, styles.and_then(|mm| mm.get(&child)), state.clone());

        match tag.as_str() {
            "g" | "a" => {
                render_children(
                    doc,
                    child,
                    child_m,
                    &child_state,
                    surf,
                    font,
                    styles,
                    depth + 1,
                );
            }
            // A nested `<svg>` / `<symbol>` establishes its own viewport at (x,y) with its own
            // viewBox — the structure SVG sprite sheets use (one inner <svg> per icon).
            "svg" | "symbol" => {
                let x = num_attr(el, "x", 0.0);
                let y = num_attr(el, "y", 0.0);
                let vb = attr(el, "viewbox").map(parse_numbers);
                let (vw, vh) = vb
                    .as_ref()
                    .filter(|v| v.len() == 4)
                    .map(|v| (v[2], v[3]))
                    .unwrap_or((0.0, 0.0));
                let w = attr(el, "width").and_then(parse_len).unwrap_or(vw);
                let h = attr(el, "height").and_then(parse_len).unwrap_or(vh);
                let inner = if w > 0.0 && h > 0.0 {
                    child_m
                        .then(Affine::translate(x, y))
                        .then(viewbox_affine(el, w, h))
                } else {
                    child_m.then(Affine::translate(x, y))
                };
                render_children(
                    doc,
                    child,
                    inner,
                    &child_state,
                    surf,
                    font,
                    styles,
                    depth + 1,
                );
            }
            // `<use xlink:href="#id">`: render the referenced element at this use's (x,y).
            "use" => {
                let href = attr(el, "href").or_else(|| attr(el, "xlink:href"));
                if let Some(id) = href.and_then(|h| h.trim().strip_prefix('#').map(str::to_string))
                {
                    if let Some(target) = find_by_id(doc, &id) {
                        if target != child {
                            let ux = num_attr(el, "x", 0.0);
                            let uy = num_attr(el, "y", 0.0);
                            let use_m = child_m.then(Affine::translate(ux, uy));
                            render_element(
                                doc,
                                target,
                                use_m,
                                &child_state,
                                surf,
                                font,
                                styles,
                                depth + 1,
                            );
                        }
                    }
                }
            }
            "rect" => {
                let x = num_attr(el, "x", 0.0);
                let y = num_attr(el, "y", 0.0);
                let w = num_attr(el, "width", 0.0);
                let h = num_attr(el, "height", 0.0);
                if w <= 0.0 || h <= 0.0 {
                    return;
                }
                let mut rx = attr(el, "rx").and_then(parse_len);
                let mut ry = attr(el, "ry").and_then(parse_len);
                if rx.is_none() {
                    rx = ry;
                }
                if ry.is_none() {
                    ry = rx;
                }
                let pts = flatten_rect(x, y, w, h, rx.unwrap_or(0.0), ry.unwrap_or(0.0), &child_m);
                paint_shape(doc, surf, &[pts], &[true], &child_state, &child_m);
            }
            "circle" => {
                let cx = num_attr(el, "cx", 0.0);
                let cy = num_attr(el, "cy", 0.0);
                let r = num_attr(el, "r", 0.0);
                if r <= 0.0 {
                    return;
                }
                let pts = flatten_ellipse(cx, cy, r, r, &child_m);
                paint_shape(doc, surf, &[pts], &[true], &child_state, &child_m);
            }
            "ellipse" => {
                let cx = num_attr(el, "cx", 0.0);
                let cy = num_attr(el, "cy", 0.0);
                let rx = num_attr(el, "rx", 0.0);
                let ry = num_attr(el, "ry", 0.0);
                if rx <= 0.0 || ry <= 0.0 {
                    return;
                }
                let pts = flatten_ellipse(cx, cy, rx, ry, &child_m);
                paint_shape(doc, surf, &[pts], &[true], &child_state, &child_m);
            }
            "line" => {
                let x1 = num_attr(el, "x1", 0.0);
                let y1 = num_attr(el, "y1", 0.0);
                let x2 = num_attr(el, "x2", 0.0);
                let y2 = num_attr(el, "y2", 0.0);
                let pts = vec![child_m.apply(x1, y1), child_m.apply(x2, y2)];
                // Lines never fill; only stroke.
                if let Some(col) = child_state.stroke_color() {
                    stroke_polyline(
                        surf,
                        &pts,
                        child_state.stroke_width * child_m.mean_scale(),
                        col,
                        false,
                    );
                }
            }
            "polyline" | "polygon" => {
                let nums = attr(el, "points").map(parse_numbers).unwrap_or_default();
                let mut pts = Vec::with_capacity(nums.len() / 2);
                let mut k = 0;
                while k + 1 < nums.len() {
                    pts.push(child_m.apply(nums[k], nums[k + 1]));
                    k += 2;
                }
                if pts.len() < 2 {
                    return;
                }
                let closed = tag == "polygon";
                paint_shape(doc, surf, &[pts], &[closed], &child_state, &child_m);
            }
            "path" => {
                if let Some(d) = attr(el, "d") {
                    let (subpaths, closed) = flatten_path(d, &child_m);
                    paint_shape(doc, surf, &subpaths, &closed, &child_state, &child_m);
                }
            }
            "text" | "tspan" => {
                if let Some(font) = font {
                    draw_text(doc, child, el, &child_m, &child_state, surf, font);
                }
                // Recurse for nested tspans.
                render_children(
                    doc,
                    child,
                    child_m,
                    &child_state,
                    surf,
                    font,
                    styles,
                    depth + 1,
                );
            }
            _ => {
                // Unknown element: recurse in case it wraps shapes (defensive, e.g. <switch>).
                render_children(
                    doc,
                    child,
                    child_m,
                    &child_state,
                    surf,
                    font,
                    styles,
                    depth + 1,
                );
            }
        }
    }
}

/// Extract the `#id` from a `url(#id)` paint value (quotes/whitespace stripped), or `None`.
fn paint_url_id(v: &str) -> Option<String> {
    let t = v.trim();
    let inner = t.strip_prefix("url(")?;
    let inner = inner.split(')').next()?.trim();
    let inner = inner.trim_matches(|c| c == '"' || c == '\'').trim();
    let id = inner.strip_prefix('#').unwrap_or(inner).trim();
    if id.is_empty() {
        None
    } else {
        Some(id.to_string())
    }
}

/// A parsed gradient color stop.
#[derive(Clone, Copy)]
struct GradStop {
    offset: f32, // 0..1
    color: Color,
    /// Whether `stop-color` was a CSS system color keyword — forced colors preserves it.
    is_system: bool,
}

enum GradKind {
    Linear { x1: f32, y1: f32, x2: f32, y2: f32 },
    Radial { cx: f32, cy: f32, r: f32 },
}

/// A resolved SVG gradient (linear/radial). Coordinates are in either userSpace or the shape's
/// objectBoundingBox (0..1) per `user_space`.
struct Gradient {
    kind: GradKind,
    stops: Vec<GradStop>,
    user_space: bool,
    transform: Affine,
}

impl Affine {
    /// Inverse of this affine, or `None` if singular (degenerate scale).
    fn invert(&self) -> Option<Affine> {
        let det = self.a * self.d - self.b * self.c;
        if det.abs() < 1e-9 {
            return None;
        }
        let inv = 1.0 / det;
        Some(Affine {
            a: self.d * inv,
            b: -self.b * inv,
            c: -self.c * inv,
            d: self.a * inv,
            e: (self.c * self.f - self.d * self.e) * inv,
            f: (self.b * self.e - self.a * self.f) * inv,
        })
    }
}

/// A gradient coordinate attribute: a raw number, or a percentage (→ fraction).
fn grad_coord(el: &dom::ElementData, name: &str, default: f32) -> f32 {
    match attr(el, name) {
        Some(s) => {
            let s = s.trim();
            if let Some(p) = s.strip_suffix('%') {
                p.trim()
                    .parse::<f32>()
                    .map(|v| v / 100.0)
                    .unwrap_or(default)
            } else {
                s.parse::<f32>().unwrap_or(default)
            }
        }
        None => default,
    }
}

/// Resolve a gradient element (by id) into a [`Gradient`]: its stops (following one level of
/// `xlink:href` stop inheritance), geometry, units, and `gradientTransform`. `None` if the id isn't a
/// gradient or has no stops.
fn resolve_gradient(doc: &Document, id: &str) -> Option<Gradient> {
    let node = find_by_id(doc, id)?;
    let el = match &doc.get(node).data {
        NodeData::Element(e) => e,
        _ => return None,
    };
    let tag = el.tag.to_ascii_lowercase();
    let is_linear = tag == "lineargradient";
    let is_radial = tag == "radialgradient";
    if !is_linear && !is_radial {
        return None;
    }
    let user_space = attr(el, "gradientunits")
        .map(|u| u.trim().eq_ignore_ascii_case("userSpaceOnUse"))
        .unwrap_or(false);
    let transform = attr(el, "gradienttransform")
        .map(parse_transform)
        .unwrap_or_else(Affine::identity);

    // Stops: this element's <stop> children, else inherited via xlink:href (one hop).
    let mut stops = collect_stops(doc, node);
    if stops.is_empty() {
        if let Some(href) = attr(el, "href").or_else(|| attr(el, "xlink:href")) {
            if let Some(rid) = href.trim().strip_prefix('#') {
                if let Some(rnode) = find_by_id(doc, rid) {
                    stops = collect_stops(doc, rnode);
                }
            }
        }
    }
    if stops.is_empty() {
        return None;
    }

    let kind = if is_linear {
        GradKind::Linear {
            x1: grad_coord(el, "x1", 0.0),
            y1: grad_coord(el, "y1", 0.0),
            x2: grad_coord(el, "x2", if user_space { 0.0 } else { 1.0 }),
            y2: grad_coord(el, "y2", 0.0),
        }
    } else {
        GradKind::Radial {
            cx: grad_coord(el, "cx", 0.5),
            cy: grad_coord(el, "cy", 0.5),
            r: grad_coord(el, "r", 0.5),
        }
    };
    Some(Gradient {
        kind,
        stops,
        user_space,
        transform,
    })
}

/// Collect `<stop>` children (offset + resolved color incl. stop-opacity) of a gradient element.
fn collect_stops(doc: &Document, grad: NodeId) -> Vec<GradStop> {
    let mut out = Vec::new();
    for &c in &doc.get(grad).children {
        let el = match &doc.get(c).data {
            NodeData::Element(e) if e.tag.eq_ignore_ascii_case("stop") => e,
            _ => continue,
        };
        let style = attr(el, "style").map(|s| s.to_string());
        let offset =
            match prop(el, "offset", &style).or_else(|| attr(el, "offset").map(str::to_string)) {
                Some(s) => {
                    let s = s.trim();
                    s.strip_suffix('%')
                        .and_then(|p| p.trim().parse::<f32>().ok().map(|v| v / 100.0))
                        .or_else(|| s.parse::<f32>().ok())
                        .unwrap_or(0.0)
                }
                None => 0.0,
            };
        let stop_color_str = prop(el, "stop-color", &style);
        let is_system = stop_color_str
            .as_deref()
            .is_some_and(|c| style::is_system_color_keyword(&c.trim().to_ascii_lowercase()));
        let mut color = stop_color_str
            .and_then(|c| parse_css_color(c.trim()))
            .unwrap_or(Color {
                r: 0,
                g: 0,
                b: 0,
                a: 255,
            });
        if let Some(o) = prop(el, "stop-opacity", &style).and_then(|o| o.trim().parse::<f32>().ok())
        {
            color = apply_alpha(color, o);
        }
        out.push(GradStop {
            offset: offset.clamp(0.0, 1.0),
            color,
            is_system,
        });
    }
    out
}

/// The gradient color at parameter `t` (0..1), interpolating between the surrounding stops.
fn gradient_color_at(stops: &[GradStop], t: f32) -> Color {
    let t = t.clamp(0.0, 1.0);
    if t <= stops[0].offset {
        return stops[0].color;
    }
    let last = stops[stops.len() - 1];
    if t >= last.offset {
        return last.color;
    }
    for w in stops.windows(2) {
        let (a, b) = (w[0], w[1]);
        if t >= a.offset && t <= b.offset {
            let span = (b.offset - a.offset).max(1e-6);
            let f = (t - a.offset) / span;
            return Color {
                r: lerp_u8(a.color.r, b.color.r, f),
                g: lerp_u8(a.color.g, b.color.g, f),
                b: lerp_u8(a.color.b, b.color.b, f),
                a: lerp_u8(a.color.a, b.color.a, f),
            };
        }
    }
    last.color
}

fn lerp_u8(a: u8, b: u8, f: f32) -> u8 {
    (a as f32 + (b as f32 - a as f32) * f)
        .round()
        .clamp(0.0, 255.0) as u8
}

/// Fill the device-space `subpaths` with a gradient. For each covered pixel, the device point is
/// mapped back to gradient space (via the inverse shape transform `m` and, for objectBoundingBox
/// units, the shape's user-space bbox), the gradient parameter `t` computed, and the interpolated
/// stop color blended. `extra_alpha` folds in fill-opacity × element opacity.
fn fill_subpaths_gradient(
    surf: &mut Surface,
    subpaths: &[Vec<(f32, f32)>],
    grad: &Gradient,
    m: &Affine,
    evenodd: bool,
    extra_alpha: f32,
) {
    // Device-space bounds of the shape.
    let (mut miny, mut maxy) = (f32::MAX, f32::MIN);
    let (mut minx, mut maxx) = (f32::MAX, f32::MIN);
    for p in subpaths {
        for &(x, y) in p {
            minx = minx.min(x);
            maxx = maxx.max(x);
            miny = miny.min(y);
            maxy = maxy.max(y);
        }
    }
    if !miny.is_finite() || !minx.is_finite() {
        return;
    }
    let inv = match m.invert() {
        Some(i) => i,
        None => return,
    };
    // For objectBoundingBox units we need the shape's user-space bbox (map device corners back).
    let (ux0, uy0) = inv.apply(minx, miny);
    let (ux1, uy1) = inv.apply(maxx, maxy);
    let (bb_x, bb_y) = (ux0.min(ux1), uy0.min(uy1));
    let (bb_w, bb_h) = ((ux1 - ux0).abs().max(1e-6), (uy1 - uy0).abs().max(1e-6));
    // Gradient geometry → user space (objectBoundingBox maps 0..1 across the bbox).
    let to_user = |gx: f32, gy: f32| -> (f32, f32) {
        if grad.user_space {
            grad.transform.apply(gx, gy)
        } else {
            let (px, py) = grad.transform.apply(gx, gy);
            (bb_x + px * bb_w, bb_y + py * bb_h)
        }
    };

    let y0 = miny.floor().max(0.0) as i32;
    let y1 = (maxy.ceil() as i32).min(surf.h as i32);
    let mut xs: Vec<(f32, i32)> = Vec::new();
    for y in y0..y1 {
        let py = y as f32 + 0.5;
        xs.clear();
        for poly in subpaths {
            let n = poly.len();
            if n < 2 {
                continue;
            }
            for i in 0..n {
                let (x0, yy0) = poly[i];
                let (x1p, yy1) = poly[(i + 1) % n];
                if (yy0 <= py && yy1 > py) || (yy1 <= py && yy0 > py) {
                    let t = (py - yy0) / (yy1 - yy0);
                    let x = x0 + t * (x1p - x0);
                    let dir = if yy1 > yy0 { 1 } else { -1 };
                    xs.push((x, dir));
                }
            }
        }
        if xs.is_empty() {
            continue;
        }
        xs.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
        // Walk spans; inside-ness per even-odd or nonzero winding.
        let mut wind = 0;
        for i in 0..xs.len() - 1 {
            if evenodd {
                wind ^= 1;
            } else {
                wind += xs[i].1;
            }
            // Inside the shape when the winding (even-odd toggle or nonzero count) is non-zero.
            if wind == 0 {
                continue;
            }
            let xa = xs[i].0;
            let xb = xs[i + 1].0;
            let xstart = xa.ceil().max(0.0) as i32;
            let xend = (xb.floor() as i32).min(surf.w as i32 - 1);
            for x in xstart..=xend {
                let (ux, uy) = inv.apply(x as f32 + 0.5, py);
                let t = match &grad.kind {
                    GradKind::Linear { x1, y1, x2, y2 } => {
                        let (gx1, gy1) = to_user(*x1, *y1);
                        let (gx2, gy2) = to_user(*x2, *y2);
                        let (dx, dy) = (gx2 - gx1, gy2 - gy1);
                        let len2 = dx * dx + dy * dy;
                        if len2 < 1e-9 {
                            0.0
                        } else {
                            ((ux - gx1) * dx + (uy - gy1) * dy) / len2
                        }
                    }
                    GradKind::Radial { cx, cy, r } => {
                        let (gcx, gcy) = to_user(*cx, *cy);
                        // Radius scales with the bbox (OBB) or stays user-space.
                        let rr = if grad.user_space {
                            *r
                        } else {
                            r * (bb_w + bb_h) * 0.5
                        };
                        if rr < 1e-6 {
                            1.0
                        } else {
                            (((ux - gcx).powi(2) + (uy - gcy).powi(2)).sqrt()) / rr
                        }
                    }
                };
                let col = apply_alpha(gradient_color_at(&grad.stops, t), extra_alpha);
                surf.blend(x, y, col);
            }
        }
    }
}

/// Fill (all subpaths together, honoring fill-rule) then stroke (each subpath) a flattened shape.
fn paint_shape(
    doc: &Document,
    surf: &mut Surface,
    subpaths: &[Vec<(f32, f32)>],
    closed: &[bool],
    state: &PaintState,
    m: &Affine,
) {
    // Gradient fill takes precedence over the solid fallback when the ref resolves.
    let mut gradient = state
        .fill_url
        .as_deref()
        .and_then(|id| resolve_gradient(doc, id));
    // Forced colors: a non-system gradient stop resolves to the shape's forced currentColor (so a
    // gradient of author colors collapses to solid); system-color stops are preserved.
    if let (Some(g), Some((r, gn, b))) = (gradient.as_mut(), state.force_stops) {
        for stop in &mut g.stops {
            if !stop.is_system {
                stop.color = Color {
                    r,
                    g: gn,
                    b,
                    a: stop.color.a,
                };
            }
        }
    }
    // A gradient whose stops are all the same color (e.g. after the forced-colors collapse above)
    // renders as a SOLID fill, so its edge anti-aliasing matches a plain solid rect (the WPT refs
    // use one) rather than the gradient rasterizer's.
    let solid_stop = gradient.as_ref().and_then(|g| {
        let first = g.stops.first()?.color;
        g.stops.iter().all(|s| s.color == first).then_some(first)
    });
    if let Some(col) = solid_stop {
        fill_subpaths(
            surf,
            subpaths,
            apply_alpha(col, state.fill_opacity * state.opacity),
            state.evenodd,
        );
    } else if let Some(g) = gradient {
        fill_subpaths_gradient(
            surf,
            subpaths,
            &g,
            m,
            state.evenodd,
            state.fill_opacity * state.opacity,
        );
    } else if let Some(col) = state.fill_color() {
        fill_subpaths(surf, subpaths, col, state.evenodd);
    }
    if let Some(col) = state.stroke_color() {
        let w = state.stroke_width * m.mean_scale();
        for (i, sp) in subpaths.iter().enumerate() {
            let is_closed = closed.get(i).copied().unwrap_or(false);
            stroke_polyline(surf, sp, w, col, is_closed);
        }
    }
}

/// Draw `<text>` content at (x,y) (alphabetic baseline) using the engine font, filled with the
/// element's fill color. Text-anchor (start/middle/end) is honored; no per-glyph kerning/rotation.
fn draw_text(
    doc: &Document,
    node: NodeId,
    el: &dom::ElementData,
    m: &Affine,
    state: &PaintState,
    surf: &mut Surface,
    font: &SystemFont,
) {
    let text = collect_text(doc, node);
    if text.trim().is_empty() {
        return;
    }
    let x = num_attr(el, "x", 0.0);
    let y = num_attr(el, "y", 0.0);
    let size = attr(el, "font-size")
        .and_then(parse_len)
        .unwrap_or(16.0)
        .max(1.0)
        * m.mean_scale();
    let (dx, dy) = m.apply(x, y);
    let col = match state.fill_color() {
        Some(c) => c,
        None => Color {
            r: 0,
            g: 0,
            b: 0,
            a: 255,
        },
    };
    let advance: f32 = text.chars().map(|ch| font.advance(ch, size)).sum();
    let anchor = attr(el, "text-anchor").unwrap_or("start");
    let mut pen = match anchor {
        "middle" => dx - advance / 2.0,
        "end" => dx - advance,
        _ => dx,
    };
    for ch in text.chars() {
        if let Some(g) = font.rasterize(ch, size) {
            for row in 0..g.height {
                for col_i in 0..g.width {
                    let cov = g.coverage[row * g.width + col_i];
                    if cov == 0 {
                        continue;
                    }
                    let gx = pen as i32 + g.left + col_i as i32;
                    let gy = dy as i32 + g.top + row as i32;
                    let cc = Color {
                        a: ((col.a as u16 * cov as u16) / 255) as u8,
                        ..col
                    };
                    surf.blend(gx, gy, cc);
                }
            }
            pen += g.advance;
        } else {
            pen += font.advance(ch, size);
        }
    }
}

/// Concatenate the direct text-node children of `node` (immediate text content of a `<text>`).
fn collect_text(doc: &Document, node: NodeId) -> String {
    let mut out = String::new();
    for &c in &doc.get(node).children {
        if let NodeData::Text(t) = &doc.get(c).data {
            out.push_str(t);
        }
    }
    // Collapse runs of whitespace to single spaces (SVG default xml:space).
    let mut s = String::with_capacity(out.len());
    let mut prev_ws = false;
    for ch in out.chars() {
        if ch.is_whitespace() {
            if !prev_ws {
                s.push(' ');
            }
            prev_ws = true;
        } else {
            s.push(ch);
            prev_ws = false;
        }
    }
    s.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_transform_translate_scale() {
        let m = parse_transform("translate(10 20) scale(2)");
        let (x, y) = m.apply(1.0, 1.0);
        assert_eq!((x, y), (12.0, 22.0));
    }

    #[test]
    fn parses_numbers_with_commas_and_signs() {
        assert_eq!(parse_numbers("0 0 10,10"), vec![0.0, 0.0, 10.0, 10.0]);
        assert_eq!(parse_numbers("-1.5e1 .5"), vec![-15.0, 0.5]);
    }

    #[test]
    fn flattens_a_triangle_path() {
        let (subs, closed) = flatten_path("M0 0 L10 0 L5 10 Z", &Affine::identity());
        assert_eq!(subs.len(), 1);
        assert!(closed[0]);
        assert_eq!(subs[0][0], (0.0, 0.0));
    }

    /// Find the `<svg>` node id in a parsed document.
    fn svg_id(doc: &Document) -> NodeId {
        (0..doc.len())
            .map(NodeId)
            .find(|&id| {
                matches!(&doc.get(id).data,
                NodeData::Element(e) if e.tag.eq_ignore_ascii_case("svg"))
            })
            .expect("an <svg> element")
    }

    /// RGBA pixel at (x,y) in a `DecodedImage`.
    fn px(img: &DecodedImage, x: u32, y: u32) -> (u8, u8, u8, u8) {
        let i = ((y * img.w + x) * 4) as usize;
        (
            img.rgba[i],
            img.rgba[i + 1],
            img.rgba[i + 2],
            img.rgba[i + 3],
        )
    }

    fn render(html: &str, w: u32, h: u32) -> DecodedImage {
        let doc = html::parse(html);
        let id = svg_id(&doc);
        rasterize_svg(&doc, id, w, h, None, None)
    }

    #[test]
    fn rect_circle_path_at_expected_locations() {
        // 100x100 svg: a red rect (top-left quadrant), a blue circle (top-right), a green
        // triangle path (bottom). Assert each primitive's color lands where expected, transparent
        // elsewhere.
        let img = render(
            r#"<svg width="100" height="100">
                 <rect x="0" y="0" width="40" height="40" fill="red"/>
                 <circle cx="75" cy="25" r="20" fill="blue"/>
                 <path d="M30 70 L70 70 L50 95 Z" fill="green"/>
               </svg>"#,
            100,
            100,
        );
        // Red rect interior.
        let (r, g, b, a) = px(&img, 20, 20);
        assert_eq!((r, g, b), (255, 0, 0), "rect should be red");
        assert_eq!(a, 255);
        // Blue circle center.
        let (r, g, b, a) = px(&img, 75, 25);
        assert_eq!((r, g, b), (0, 0, 255), "circle should be blue");
        assert_eq!(a, 255);
        // Green triangle interior (near its top edge center).
        let (r, g, b, a) = px(&img, 50, 75);
        assert_eq!((r, g, b), (0, 128, 0), "triangle should be green");
        assert_eq!(a, 255);
        // Empty corner is transparent.
        let (.., a) = px(&img, 95, 95);
        assert_eq!(a, 0, "empty area transparent");
    }

    #[test]
    fn viewbox_scales_shape_to_fill() {
        // A rect filling a 10x10 viewBox should fill a 100x100 svg after scale-to-fit.
        let img = render(
            r#"<svg width="100" height="100" viewBox="0 0 10 10">
                 <rect x="0" y="0" width="10" height="10" fill="red"/>
               </svg>"#,
            100,
            100,
        );
        // Center and a far corner should both be red (the 10x10 shape scaled 10x covers the box).
        assert_eq!(px(&img, 50, 50).0, 255);
        assert_eq!(px(&img, 95, 95).0, 255);
        assert_eq!(px(&img, 95, 95).3, 255);
    }

    #[test]
    fn group_transform_translate_offsets_shape() {
        // A rect at (0,0) inside a g translated by (50,50) must appear at the box center, not origin.
        let img = render(
            r#"<svg width="100" height="100">
                 <g transform="translate(50,50)">
                   <rect x="0" y="0" width="20" height="20" fill="red"/>
                 </g>
               </svg>"#,
            100,
            100,
        );
        // Origin is now empty (transparent), the translated location is red.
        assert_eq!(
            px(&img, 5, 5).3,
            0,
            "origin should be empty after translate"
        );
        assert_eq!(
            px(&img, 55, 55),
            (255, 0, 0, 255),
            "rect shifted to (50,50)"
        );
    }

    #[test]
    fn stroke_only_circle_outline_no_fill() {
        // fill:none stroke: the center is transparent, the rim is colored.
        let img = render(
            r#"<svg width="100" height="100">
                 <circle cx="50" cy="50" r="40" fill="none" stroke="black" stroke-width="4"/>
               </svg>"#,
            100,
            100,
        );
        assert_eq!(px(&img, 50, 50).3, 0, "unfilled center transparent");
        // A point on the rim (top, y≈10) should be black-ish.
        let rim = px(&img, 50, 10);
        assert!(rim.3 > 0, "rim should be stroked");
    }

    #[test]
    fn fill_opacity_is_applied() {
        let img = render(
            r#"<svg width="20" height="20">
                 <rect x="0" y="0" width="20" height="20" fill="red" fill-opacity="0.5"/>
               </svg>"#,
            20,
            20,
        );
        let (r, _, _, a) = px(&img, 10, 10);
        assert_eq!(r, 255);
        assert!((120..=135).contains(&a), "alpha ~128, got {a}");
    }

    #[test]
    fn handles_no_attrs_without_crashing() {
        // Degenerate: empty svg, zero-size shapes, unknown elements — must not panic.
        let img = render(
            r##"<svg width="10" height="10">
                 <rect/><circle/><defs><linearGradient/></defs><use href="#x"/>
                 <path d="garbage Z M"/>
               </svg>"##,
            10,
            10,
        );
        assert_eq!(img.w, 10);
        assert_eq!(px(&img, 5, 5).3, 0);
    }

    #[test]
    fn arc_command_renders_pixels() {
        // A path using an arc (A) command should produce filled pixels (arc flattening works).
        let img = render(
            r#"<svg width="50" height="50">
                 <path d="M5 25 A20 20 0 1 1 45 25 L45 45 L5 45 Z" fill="red"/>
               </svg>"#,
            50,
            50,
        );
        // Somewhere inside the half-disc + base should be red.
        assert_eq!(px(&img, 25, 35).0, 255, "arc-bounded shape filled");
    }

    #[test]
    fn path_packed_decimals_parse_as_separate_numbers() {
        // SVG path data may omit separators where a second `.` starts a new number: `.5.5` = 0.5,0.5.
        // Square corners (0.5,0.5),(19.5,0.5),(19.5,19.5),(0.5,19.5). Before the fix the tokenizer
        // swallowed both dots into one unparseable token and the path produced nothing.
        let img = render(
            r#"<svg width="20" height="20"><path d="M.5.5 19.5.5 19.5 19.5.5 19.5Z" fill="red"/></svg>"#,
            20,
            20,
        );
        assert_eq!(
            px(&img, 10, 10).0,
            255,
            "packed-decimal path coords should produce a filled square"
        );
    }

    #[test]
    fn nested_svg_positions_and_scales_inner_viewport() {
        // Outer 100x100; an inner <svg> at (10,10) sized 40x40 with its own 0..10 viewBox holds a
        // full red rect. The inner content should fill (10,10)..(50,50) and nothing outside it.
        let img = render(
            r#"<svg width="100" height="100" viewBox="0 0 100 100">
                 <svg x="10" y="10" width="40" height="40" viewBox="0 0 10 10">
                   <rect width="10" height="10" fill="red"/>
                 </svg>
               </svg>"#,
            100,
            100,
        );
        assert_eq!(
            px(&img, 30, 30).0,
            255,
            "inner rect red inside the nested viewport"
        );
        assert_eq!(px(&img, 30, 30).3, 255);
        assert_eq!(
            px(&img, 5, 5).3,
            0,
            "above/left of the nested viewport is empty"
        );
        assert_eq!(
            px(&img, 70, 70).3,
            0,
            "below/right of the nested viewport is empty"
        );
    }

    #[test]
    fn use_renders_referenced_element_at_offset() {
        // A <rect> defined in <defs> is drawn by <use> translated to (60,60).
        let img = render(
            r##"<svg width="100" height="100" viewBox="0 0 100 100" xmlns:xlink="http://www.w3.org/1999/xlink">
                 <defs><rect id="r" width="20" height="20" fill="blue"/></defs>
                 <use xlink:href="#r" x="60" y="60"/>
               </svg>"##,
            100,
            100,
        );
        assert_eq!(
            px(&img, 70, 70),
            (0, 0, 255, 255),
            "use'd rect drawn at its offset"
        );
        assert_eq!(
            px(&img, 10, 10).3,
            0,
            "the original defs rect is not drawn in place"
        );
    }

    #[test]
    fn linear_gradient_fill_interpolates_left_to_right() {
        // A 100x100 rect filled with a userSpaceOnUse red→blue horizontal gradient: left edge red,
        // right edge blue.
        let img = render(
            r##"<svg width="100" height="100" viewBox="0 0 100 100">
                 <defs>
                   <linearGradient id="g" gradientUnits="userSpaceOnUse" x1="0" y1="0" x2="100" y2="0">
                     <stop offset="0%" stop-color="#ff0000"/>
                     <stop offset="100%" stop-color="#0000ff"/>
                   </linearGradient>
                 </defs>
                 <rect width="100" height="100" fill="url(#g)"/>
               </svg>"##,
            100,
            100,
        );
        let left = px(&img, 3, 50);
        let right = px(&img, 96, 50);
        assert!(left.0 > 200 && left.2 < 60, "left edge red, got {left:?}");
        assert!(
            right.2 > 200 && right.0 < 60,
            "right edge blue, got {right:?}"
        );
        assert_eq!(left.3, 255);
    }

    #[test]
    fn use_references_group_with_transform() {
        // <use> of a <g> containing a rect, with the use translated — the group's contents appear
        // at the use offset (covers the Commons-logo pattern of <use> + transformed groups).
        let img = render(
            r##"<svg width="100" height="100" viewBox="0 0 100 100" xmlns:xlink="http://www.w3.org/1999/xlink">
                 <defs><g id="g"><rect width="10" height="10" fill="red"/></g></defs>
                 <use xlink:href="#g" x="40" y="40"/>
               </svg>"##,
            100,
            100,
        );
        assert_eq!(px(&img, 44, 44).0, 255, "group drawn via use at offset");
    }
}
