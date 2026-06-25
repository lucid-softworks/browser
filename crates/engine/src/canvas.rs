//! Canvas 2D rasterizer adapter.
//!
//! The actual rasterizer now lives in `paint::canvas` (engine-agnostic: text glyphs come through the
//! `paint::GlyphRasterizer` trait and the output is a paint-local `RasterImage`). This module is a
//! thin shim that re-exports the parser / display-list types and adapts `rasterize_canvas` to take
//! the engine's [`SystemFont`] and return the engine's [`DecodedImage`].

use std::collections::HashMap;

use paint::Color;

use crate::font::SystemFont;
use crate::DecodedImage;

// Re-export the display-list parser + types from paint so the rest of the engine keeps using
// `crate::canvas::{parse_canvas_lists, CanvasList}` unchanged.
pub use paint::canvas::{parse_canvas_lists, CanvasList};

/// Public re-export of the canvas CSS color parser so the SVG module can reuse it (named/hex/
/// rgb/hsl/transparent) instead of duplicating the table.
pub fn parse_css_color_pub(s: &str) -> Option<Color> {
    paint::canvas::parse_css_color_pub(s)
}

/// Rasterize one canvas's display list into a straight-alpha RGBA [`DecodedImage`] of
/// `width`×`height` pixels (the canvas's pixel buffer; the engine scales it to the box's CSS size).
///
/// Wraps [`paint::canvas::rasterize_canvas`], passing the engine's [`SystemFont`] (which implements
/// `paint::GlyphRasterizer`) for the `text` op and converting the paint-local result into the
/// engine's `DecodedImage`.
pub fn rasterize_canvas(
    cv: &CanvasList,
    font: Option<&SystemFont>,
    sources: &HashMap<usize, (&[u8], u32, u32)>,
) -> DecodedImage {
    let font = font.map(|f| f as &dyn paint::GlyphRasterizer);
    let img = paint::canvas::rasterize_canvas(cv, font, sources);
    DecodedImage {
        rgba: img.rgba,
        w: img.w,
        h: img.h,
    }
}
