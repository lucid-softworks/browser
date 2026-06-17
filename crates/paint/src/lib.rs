//! Software compositor: an RGBA framebuffer plus the primitives we paint into it.
//!
//! Everything here is ours. The only thing we ever intend to borrow from a crate is
//! *glyph rasterization* (turning a font + size into a coverage bitmap), and that lives
//! behind the [`GlyphRasterizer`] trait so it can be replaced with a hand-written
//! rasterizer later without touching the rest of paint.

/// A straight-alpha RGBA8 pixel buffer. `stride` is bytes per row (>= width * 4).
pub struct Framebuffer {
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub pixels: Vec<u8>,
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
        Self { width, height, stride, pixels }
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

    /// Source-over fill of an axis-aligned rect, clipped to the framebuffer.
    pub fn fill_rect(&mut self, rect: Rect, c: Color) {
        let x0 = rect.x.max(0);
        let y0 = rect.y.max(0);
        let x1 = (rect.x + rect.w).min(self.width as i32);
        let y1 = (rect.y + rect.h).min(self.height as i32);
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

    /// Blend a single coverage value (0..=255) of `c` at one pixel. Used by text painting
    /// once a [`GlyphRasterizer`] hands us coverage bitmaps.
    pub fn blend_coverage(&mut self, x: i32, y: i32, coverage: u8, c: Color) {
        if x < 0 || y < 0 || x >= self.width as i32 || y >= self.height as i32 {
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
        fb.fill_rect(Rect { x: -1, y: -1, w: 2, h: 2 }, Color::rgb(10, 20, 30));
        // Pixel (0,0) should be painted; clipped negative region ignored.
        assert_eq!(&fb.pixels[0..4], &[10, 20, 30, 255]);
        // Pixel (2,2) untouched (still opaque black).
        let i = (2 * fb.stride + 2 * 4) as usize;
        assert_eq!(&fb.pixels[i..i + 4], &[0, 0, 0, 255]);
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
