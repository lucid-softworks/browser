//! Software compositor: an RGBA framebuffer plus the primitives we paint into it.
//!
//! Everything here is ours. The only thing we ever intend to borrow from a crate is
//! *glyph rasterization* (turning a font + size into a coverage bitmap), and that lives
//! behind the [`GlyphRasterizer`] trait so it can be replaced with a hand-written
//! rasterizer later without touching the rest of paint.

/// Canvas 2D display-list rasterizer (turns JS `<canvas>` command lists into RGBA bitmaps).
pub mod canvas;

/// A straight-alpha RGBA8 pixel buffer. `stride` is bytes per row (>= width * 4).
pub struct Framebuffer {
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub pixels: Vec<u8>,
    /// Optional clip rectangle (device px): when set, all primitives are additionally clipped to it
    /// (intersected with the framebuffer bounds). Drives CSS `overflow: hidden`/`clip`/`scroll`. The
    /// painter saves/restores it around a clipping box's subtree.
    pub clip: Option<Rect>,
}

/// An RGBA color, 0..=255 per channel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

impl Color {
    pub const fn rgb(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b, a: 255 }
    }
    pub const WHITE: Color = Color::rgb(255, 255, 255);
    pub const BLACK: Color = Color::rgb(0, 0, 0);
}

/// An axis-aligned rectangle in device pixels.
#[derive(Clone, Copy, Debug)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl Framebuffer {
    /// Allocate a fully-opaque black framebuffer of the given size.
    pub fn new(width: u32, height: u32) -> Self {
        let stride = width * 4;
        let mut pixels = vec![0u8; (stride * height) as usize];
        // Opaque alpha so a CGImage built from this isn't fully transparent.
        for px in pixels.chunks_exact_mut(4) {
            px[3] = 255;
        }
        Self {
            width,
            height,
            stride,
            pixels,
            clip: None,
        }
    }

    /// The effective drawable bounds `(x0, y0, x1, y1)`: the framebuffer rect intersected with the
    /// current clip rect (if any).
    #[inline]
    fn bounds(&self) -> (i32, i32, i32, i32) {
        let (mut x0, mut y0) = (0i32, 0i32);
        let (mut x1, mut y1) = (self.width as i32, self.height as i32);
        if let Some(c) = self.clip {
            x0 = x0.max(c.x);
            y0 = y0.max(c.y);
            x1 = x1.min(c.x + c.w);
            y1 = y1.min(c.y + c.h);
        }
        (x0, y0, x1, y1)
    }

    /// True if device pixel `(x, y)` is inside the current clip (and the framebuffer).
    #[inline]
    fn in_clip(&self, x: i32, y: i32) -> bool {
        let (x0, y0, x1, y1) = self.bounds();
        x >= x0 && y >= y0 && x < x1 && y < y1
    }

    /// Fill the whole buffer with a solid color.
    pub fn clear(&mut self, c: Color) {
        for px in self.pixels.chunks_exact_mut(4) {
            px[0] = c.r;
            px[1] = c.g;
            px[2] = c.b;
            px[3] = c.a;
        }
    }

    /// Source-over fill of an axis-aligned rect, clipped to the framebuffer (and any clip rect).
    pub fn fill_rect(&mut self, rect: Rect, c: Color) {
        let (cx0, cy0, cx1, cy1) = self.bounds();
        let x0 = rect.x.max(cx0);
        let y0 = rect.y.max(cy0);
        let x1 = (rect.x + rect.w).min(cx1);
        let y1 = (rect.y + rect.h).min(cy1);
        if x1 <= x0 || y1 <= y0 {
            return;
        }
        for y in y0..y1 {
            let row = (y as u32 * self.stride) as usize;
            for x in x0..x1 {
                let i = row + (x as usize) * 4;
                blend_over(&mut self.pixels[i..i + 4], c);
            }
        }
    }

    /// Source-over fill of an axis-aligned rect with rounded corners of `radius` px, clipped to
    /// the framebuffer. The radius is clamped to half the smaller side. Pixels outside the
    /// quarter-circle at each corner are skipped; a 1px anti-aliased band softens the corner edge
    /// (coverage scaled by how far inside the circle the pixel center lies). A radius of 0 is a
    /// plain [`fill_rect`](Self::fill_rect).
    pub fn fill_round_rect(&mut self, rect: Rect, radius: f32, c: Color) {
        if radius <= 0.0 {
            self.fill_rect(rect, c);
            return;
        }
        let r = radius
            .min(rect.w as f32 / 2.0)
            .min(rect.h as f32 / 2.0)
            .max(0.0);
        if r <= 0.0 {
            self.fill_rect(rect, c);
            return;
        }
        let (cx0, cy0, cx1, cy1) = self.bounds();
        let x0 = rect.x.max(cx0);
        let y0 = rect.y.max(cy0);
        let x1 = (rect.x + rect.w).min(cx1);
        let y1 = (rect.y + rect.h).min(cy1);
        if x1 <= x0 || y1 <= y0 {
            return;
        }
        // Corner circle centers (in device pixels), inset by the radius from each corner.
        let left_cx = rect.x as f32 + r;
        let right_cx = (rect.x + rect.w) as f32 - r;
        let top_cy = rect.y as f32 + r;
        let bottom_cy = (rect.y + rect.h) as f32 - r;
        for y in y0..y1 {
            let py = y as f32 + 0.5;
            let row = (y as u32 * self.stride) as usize;
            for x in x0..x1 {
                let px = x as f32 + 0.5;
                // Determine which corner region (if any) this pixel is in.
                let cx = if px < left_cx {
                    Some(left_cx)
                } else if px > right_cx {
                    Some(right_cx)
                } else {
                    None
                };
                let cy = if py < top_cy {
                    Some(top_cy)
                } else if py > bottom_cy {
                    Some(bottom_cy)
                } else {
                    None
                };
                let coverage = match (cx, cy) {
                    (Some(cx), Some(cy)) => {
                        // In a corner: distance from the circle center, with 1px AA falloff.
                        let dist = ((px - cx).powi(2) + (py - cy).powi(2)).sqrt();
                        let edge = r - dist; // >0 inside, <0 outside
                        if edge >= 0.5 {
                            255
                        } else if edge <= -0.5 {
                            0
                        } else {
                            (((edge + 0.5) * 255.0).round()).clamp(0.0, 255.0) as u8
                        }
                    }
                    // Straight edges / interior: fully covered.
                    _ => 255,
                };
                if coverage == 0 {
                    continue;
                }
                let i = row + (x as usize) * 4;
                let cc = Color {
                    a: ((c.a as u16 * coverage as u16) / 255) as u8,
                    ..c
                };
                blend_over(&mut self.pixels[i..i + 4], cc);
            }
        }
    }

    /// Blit a decoded straight-alpha RGBA8 image into the destination rect `dst`, scaling with
    /// nearest-neighbour sampling and source-over compositing each pixel. `src` is
    /// `src_w * src_h * 4` bytes; out-of-range / empty inputs are ignored. The destination is
    /// clipped to the framebuffer.
    pub fn blit_rgba(&mut self, dst: Rect, src: &[u8], src_w: u32, src_h: u32) {
        if src_w == 0 || src_h == 0 || dst.w <= 0 || dst.h <= 0 {
            return;
        }
        if src.len() < (src_w as usize) * (src_h as usize) * 4 {
            return;
        }
        let (cx0, cy0, cx1, cy1) = self.bounds();
        let x0 = dst.x.max(cx0);
        let y0 = dst.y.max(cy0);
        let x1 = (dst.x + dst.w).min(cx1);
        let y1 = (dst.y + dst.h).min(cy1);
        if x1 <= x0 || y1 <= y0 {
            return;
        }
        for y in y0..y1 {
            // Map this destination row back to a source row (nearest-neighbour).
            let sy = (((y - dst.y) as i64 * src_h as i64) / dst.h as i64) as u32;
            let sy = sy.min(src_h - 1);
            let drow = (y as u32 * self.stride) as usize;
            let srow = (sy * src_w) as usize * 4;
            for x in x0..x1 {
                let sx = (((x - dst.x) as i64 * src_w as i64) / dst.w as i64) as u32;
                let sx = sx.min(src_w - 1);
                let si = srow + (sx as usize) * 4;
                let c = Color {
                    r: src[si],
                    g: src[si + 1],
                    b: src[si + 2],
                    a: src[si + 3],
                };
                let di = drow + (x as usize) * 4;
                blend_over(&mut self.pixels[di..di + 4], c);
            }
        }
    }

    /// Blend a single coverage value (0..=255) of `c` at one pixel. Used by text painting
    /// once a [`GlyphRasterizer`] hands us coverage bitmaps.
    pub fn blend_coverage(&mut self, x: i32, y: i32, coverage: u8, c: Color) {
        if !self.in_clip(x, y) {
            return;
        }
        let i = (y as u32 * self.stride) as usize + (x as usize) * 4;
        let scaled = Color {
            a: ((c.a as u16 * coverage as u16) / 255) as u8,
            ..c
        };
        blend_over(&mut self.pixels[i..i + 4], scaled);
    }
}

/// Straight-alpha source-over compositing of `src` onto one RGBA destination pixel.
fn blend_over(dst: &mut [u8], src: Color) {
    if src.a == 255 {
        dst[0] = src.r;
        dst[1] = src.g;
        dst[2] = src.b;
        dst[3] = 255;
        return;
    }
    if src.a == 0 {
        return;
    }
    let sa = src.a as u32;
    let ia = 255 - sa;
    dst[0] = ((src.r as u32 * sa + dst[0] as u32 * ia) / 255) as u8;
    dst[1] = ((src.g as u32 * sa + dst[1] as u32 * ia) / 255) as u8;
    dst[2] = ((src.b as u32 * sa + dst[2] as u32 * ia) / 255) as u8;
    dst[3] = (sa + dst[3] as u32 * ia / 255).min(255) as u8;
}

/// A rasterized glyph: a coverage (alpha) bitmap plus where to place it relative to the
/// pen position and how far to advance afterwards. Units are device pixels.
pub struct GlyphBitmap {
    pub width: usize,
    pub height: usize,
    pub left: i32,
    pub top: i32,
    pub advance: f32,
    pub coverage: Vec<u8>,
}

/// Abstraction over whatever turns text into pixels. Today this is backed by a reused
/// crate; the eventual all-Rust rewrite swaps the implementation, not the callers.
pub trait GlyphRasterizer {
    /// Rasterize a single character at `px` pixels. Returns `None` for glyphs with no
    /// outline (e.g. space) — callers should still advance the pen by `advance`.
    fn rasterize(&self, ch: char, px: f32) -> Option<GlyphBitmap>;
    /// Horizontal advance for a character at `px` pixels (used for spaces / metrics).
    fn advance(&self, ch: char, px: f32) -> f32;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fill_rect_is_clipped_and_opaque() {
        let mut fb = Framebuffer::new(4, 4);
        fb.fill_rect(
            Rect {
                x: -1,
                y: -1,
                w: 2,
                h: 2,
            },
            Color::rgb(10, 20, 30),
        );
        // Pixel (0,0) should be painted; clipped negative region ignored.
        assert_eq!(&fb.pixels[0..4], &[10, 20, 30, 255]);
        // Pixel (2,2) untouched (still opaque black).
        let i = (2 * fb.stride + 2 * 4) as usize;
        assert_eq!(&fb.pixels[i..i + 4], &[0, 0, 0, 255]);
    }

    #[test]
    fn clip_rect_limits_fills_and_glyphs() {
        let mut fb = Framebuffer::new(8, 8);
        // Clip to a 2x2 region at (2,2). A full-buffer fill only paints inside it.
        fb.clip = Some(Rect {
            x: 2,
            y: 2,
            w: 2,
            h: 2,
        });
        fb.fill_rect(
            Rect {
                x: 0,
                y: 0,
                w: 8,
                h: 8,
            },
            Color::rgb(9, 9, 9),
        );
        // A glyph coverage pixel outside the clip is dropped too.
        fb.blend_coverage(0, 0, 255, Color::rgb(1, 2, 3));
        let at = |fb: &Framebuffer, x: u32, y: u32| {
            let i = (y * fb.stride + x * 4) as usize;
            [fb.pixels[i], fb.pixels[i + 1], fb.pixels[i + 2]]
        };
        assert_eq!(at(&fb, 2, 2), [9, 9, 9], "inside clip painted");
        assert_eq!(at(&fb, 3, 3), [9, 9, 9], "inside clip painted");
        assert_eq!(at(&fb, 0, 0), [0, 0, 0], "outside clip + glyph dropped");
        assert_eq!(at(&fb, 4, 4), [0, 0, 0], "outside clip untouched");
    }

    #[test]
    fn blit_rgba_scales_with_nearest_neighbour() {
        // A 2x2 source: red, green / blue, white. Blit it filling a 4x4 buffer.
        let mut fb = Framebuffer::new(4, 4);
        fb.clear(Color::BLACK);
        let src: Vec<u8> = vec![
            255, 0, 0, 255, /* red   */ 0, 255, 0, 255, /* green */
            0, 0, 255, 255, /* blue  */ 255, 255, 255, 255, /* white */
        ];
        fb.blit_rgba(
            Rect {
                x: 0,
                y: 0,
                w: 4,
                h: 4,
            },
            &src,
            2,
            2,
        );
        let px = |x: usize, y: usize| {
            let i = (y as u32 * fb.stride) as usize + x * 4;
            &fb.pixels[i..i + 4]
        };
        // Each source pixel maps to the corresponding 2x2 quadrant.
        assert_eq!(px(0, 0), &[255, 0, 0, 255]); // top-left = red
        assert_eq!(px(3, 0), &[0, 255, 0, 255]); // top-right = green
        assert_eq!(px(0, 3), &[0, 0, 255, 255]); // bottom-left = blue
        assert_eq!(px(3, 3), &[255, 255, 255, 255]); // bottom-right = white
    }

    #[test]
    fn blit_rgba_composites_alpha_and_clips() {
        let mut fb = Framebuffer::new(2, 2);
        fb.clear(Color::BLACK);
        // A 1x1 half-transparent white image blitted over the whole (black) buffer.
        let src = vec![255u8, 255, 255, 128];
        // Destination extends past the buffer; must be clipped without panicking.
        fb.blit_rgba(
            Rect {
                x: 0,
                y: 0,
                w: 10,
                h: 10,
            },
            &src,
            1,
            1,
        );
        // ~50% white over black.
        assert!(fb.pixels[0] > 120 && fb.pixels[0] < 135);
    }

    #[test]
    fn round_rect_corner_is_clear_center_filled() {
        // A 16x16 rounded rect with a large radius. The very corner pixel is outside the
        // quarter-circle (untouched black), while the center is fully filled.
        let mut fb = Framebuffer::new(16, 16);
        fb.clear(Color::BLACK);
        fb.fill_round_rect(
            Rect {
                x: 0,
                y: 0,
                w: 16,
                h: 16,
            },
            8.0,
            Color::rgb(255, 0, 0),
        );
        let px = |x: usize, y: usize| {
            let i = (y as u32 * fb.stride) as usize + x * 4;
            &fb.pixels[i..i + 4]
        };
        // Top-left corner pixel: outside the circle → still black.
        assert_eq!(px(0, 0), &[0, 0, 0, 255]);
        // Center: fully red.
        assert_eq!(px(8, 8), &[255, 0, 0, 255]);
    }

    #[test]
    fn round_rect_zero_radius_is_plain_fill() {
        let mut fb = Framebuffer::new(4, 4);
        fb.clear(Color::BLACK);
        fb.fill_round_rect(
            Rect {
                x: 0,
                y: 0,
                w: 4,
                h: 4,
            },
            0.0,
            Color::rgb(10, 20, 30),
        );
        assert_eq!(&fb.pixels[0..4], &[10, 20, 30, 255]);
    }

    #[test]
    fn opacity_scaled_fill_blends() {
        // Filling with a half-alpha color over black yields ~50% gray (proves alpha scaling for
        // the opacity-threading path, which pre-scales each fill's alpha).
        let mut fb = Framebuffer::new(1, 1);
        fb.clear(Color::BLACK);
        fb.fill_rect(
            Rect {
                x: 0,
                y: 0,
                w: 1,
                h: 1,
            },
            Color {
                r: 255,
                g: 255,
                b: 255,
                a: 128,
            },
        );
        assert!(
            fb.pixels[0] > 120 && fb.pixels[0] < 135,
            "got {}",
            fb.pixels[0]
        );
    }

    #[test]
    fn coverage_blends_halfway() {
        let mut fb = Framebuffer::new(1, 1);
        fb.clear(Color::BLACK);
        fb.blend_coverage(0, 0, 128, Color::WHITE);
        // ~50% white over black.
        assert!(fb.pixels[0] > 120 && fb.pixels[0] < 135);
    }
}
