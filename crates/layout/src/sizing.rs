use crate::*;
use std::collections::HashMap;

/// Positioning context threaded through layout:
/// - `positioned`: the padding box of the nearest positioned ancestor (the containing block for
///   `position: absolute` descendants). Starts as the viewport (the initial containing block).
/// - `viewport`: the viewport rect (the containing block for `position: fixed`).
#[derive(Debug, Clone, Copy)]
pub(crate) struct Ctx {
    pub(crate) positioned: Rect,
    pub(crate) viewport: Rect,
}

/// The explicit content width set on a box's node (if any).
pub(crate) fn explicit_width(
    boxx: &LayoutBox,
    styles: &HashMap<dom::NodeId, style::ComputedStyle>,
) -> Option<f32> {
    boxx.node
        .and_then(|n| styles.get(&n))
        .and_then(|cs| cs.width)
}

/// The used content width: an explicit px `width`, or a percentage `width` resolved against the
/// containing block's content width (`cb_width`). `None` when width is `auto`.
pub(crate) fn resolved_width(
    boxx: &LayoutBox,
    styles: &HashMap<dom::NodeId, style::ComputedStyle>,
    cb_width: f32,
) -> Option<f32> {
    boxx.node.and_then(|n| styles.get(&n)).and_then(|cs| {
        cs.width
            .or_else(|| cs.width_pct.map(|p| (cb_width * p).max(0.0)))
    })
}

/// The explicit content height set on a box's node (if any).
pub(crate) fn explicit_height(
    boxx: &LayoutBox,
    styles: &HashMap<dom::NodeId, style::ComputedStyle>,
) -> Option<f32> {
    boxx.node
        .and_then(|n| styles.get(&n))
        .and_then(|cs| cs.height)
}

/// The computed style for a box's node, if any.
pub(crate) fn style_of<'a>(
    boxx: &LayoutBox,
    styles: &'a HashMap<dom::NodeId, style::ComputedStyle>,
) -> Option<&'a style::ComputedStyle> {
    boxx.node.and_then(|n| styles.get(&n))
}

/// Clamp a used content `width` to the box's `[min-width, max-width]` (resolved against the
/// containing block content width `cb_width`). `max-width` applies first per CSS, then
/// `min-width` (so min wins on conflict). A box with no node leaves the width unchanged.
pub(crate) fn clamp_width(
    boxx: &LayoutBox,
    width: f32,
    cb_width: f32,
    styles: &HashMap<dom::NodeId, style::ComputedStyle>,
) -> f32 {
    let cs = match style_of(boxx, styles) {
        Some(cs) => cs,
        None => return width,
    };
    let mut w = width;
    if let Some(max) = cs.max_width {
        w = w.min(max.resolve(cb_width));
    }
    if let Some(min) = cs.min_width {
        w = w.max(min.resolve(cb_width));
    }
    w.max(0.0)
}

/// Clamp a used content `height` to the box's `[min-height, max-height]` (resolved against the
/// containing block height `cb_height`). Percentages of an indefinite container height resolve
/// against `cb_height` (which may be 0 → percentage min/max effectively unset).
pub(crate) fn clamp_height(
    boxx: &LayoutBox,
    height: f32,
    cb_height: f32,
    styles: &HashMap<dom::NodeId, style::ComputedStyle>,
) -> f32 {
    let cs = match style_of(boxx, styles) {
        Some(cs) => cs,
        None => return height,
    };
    let mut h = height;
    if let Some(max) = cs.max_height {
        h = h.min(max.resolve(cb_height));
    }
    if let Some(min) = cs.min_height {
        h = h.max(min.resolve(cb_height));
    }
    h.max(0.0)
}

/// The `display` mode of a box (defaults to Block for anonymous/root boxes).
///
/// Reconciles the legacy `display_block`/`display_none` flags with the richer `display` enum:
/// a style constructed the old way (only `display_block: true`) still lays out as a block, and
/// `display_none` always wins. This keeps externally-constructed `ComputedStyle`s working.
pub(crate) fn display_of(
    boxx: &LayoutBox,
    styles: &HashMap<dom::NodeId, style::ComputedStyle>,
) -> style::Display {
    match style_of(boxx, styles) {
        None => style::Display::Block, // anonymous / root
        Some(cs) => {
            if cs.display_none {
                style::Display::None
            } else if cs.display == style::Display::Inline && cs.display_block {
                // Legacy flag set without the enum being updated.
                style::Display::Block
            } else {
                cs.display
            }
        }
    }
}

/// Compute an image's content-box size (width, height) from any CSS `width`/`height` and its
/// intrinsic size. Rules:
///   * both CSS dimensions set → use them;
///   * one CSS dimension set + an intrinsic aspect ratio known → scale the other to preserve it;
///   * one CSS dimension set, no intrinsic → use it for that axis, 0 for the other (skipped);
///   * no CSS dimensions → use the intrinsic size, or (0,0) if unknown.
pub(crate) fn image_content_size(
    css_w: Option<f32>,
    css_h: Option<f32>,
    intrinsic: Option<(f32, f32)>,
) -> (f32, f32) {
    match (css_w, css_h) {
        (Some(w), Some(h)) => (w.max(0.0), h.max(0.0)),
        (Some(w), None) => {
            let h = match intrinsic {
                Some((iw, ih)) if iw > 0.0 => w * (ih / iw),
                _ => 0.0,
            };
            (w.max(0.0), h.max(0.0))
        }
        (None, Some(h)) => {
            let w = match intrinsic {
                Some((iw, ih)) if ih > 0.0 => h * (iw / ih),
                _ => 0.0,
            };
            (w.max(0.0), h.max(0.0))
        }
        (None, None) => match intrinsic {
            Some((iw, ih)) => (iw.max(0.0), ih.max(0.0)),
            None => (0.0, 0.0),
        },
    }
}

/// True if an Image box is block-level (computed display block/flex/grid, the legacy
/// `display_block` flag, or out-of-flow). Otherwise the image is atomic inline-level.
pub(crate) fn image_is_block(
    boxx: &LayoutBox,
    styles: &HashMap<dom::NodeId, style::ComputedStyle>,
) -> bool {
    match style_of(boxx, styles) {
        None => false,
        Some(cs) => {
            let block_display = matches!(
                cs.display,
                style::Display::Block | style::Display::Flex | style::Display::Grid
            ) || (cs.display == style::Display::Inline && cs.display_block);
            let out_of_flow = matches!(
                cs.position,
                style::Position::Absolute | style::Position::Fixed
            );
            block_display || out_of_flow
        }
    }
}

/// True for `display` values that produce a block-level box in their parent's flow (for box-tree
/// construction purposes): block/flex/grid, plus `table` (a table box is block-level) and the
/// table-internal display types (`table-row`, `table-cell`, row groups, caption, columns). The
/// table-internal boxes are kept as structural (Block-content) boxes so `layout_table` can walk
/// them; they are never wrapped in anonymous blocks because they only appear under a table.
pub(crate) fn is_block_level_display(d: style::Display) -> bool {
    matches!(
        d,
        style::Display::Block
            | style::Display::Flex
            | style::Display::Grid
            | style::Display::Table
            | style::Display::TableRow
            | style::Display::TableCell
            | style::Display::TableRowGroup
            | style::Display::TableHeaderGroup
            | style::Display::TableFooterGroup
            | style::Display::TableCaption
            | style::Display::TableColumn
            | style::Display::TableColumnGroup
    )
}

/// The `position` of a box (defaults to Static).
pub(crate) fn position_of(
    boxx: &LayoutBox,
    styles: &HashMap<dom::NodeId, style::ComputedStyle>,
) -> style::Position {
    style_of(boxx, styles)
        .map(|cs| cs.position)
        .unwrap_or(style::Position::Static)
}

/// True if a box is taken out of normal flow (absolutely or fixed positioned).
pub(crate) fn is_out_of_flow(
    boxx: &LayoutBox,
    styles: &HashMap<dom::NodeId, style::ComputedStyle>,
) -> bool {
    matches!(
        position_of(boxx, styles),
        style::Position::Absolute | style::Position::Fixed
    )
}

/// This box's `float` (defaults to `None`). The cascade already clears `float` on absolutely /
/// fixed positioned boxes, so a non-`None` result here means a real, in-flow-affecting float.
pub(crate) fn float_of(
    boxx: &LayoutBox,
    styles: &HashMap<dom::NodeId, style::ComputedStyle>,
) -> style::Float {
    style_of(boxx, styles)
        .map(|cs| cs.float)
        .unwrap_or(style::Float::None)
}

/// This box's `clear` (defaults to `None`).
pub(crate) fn clear_of(
    boxx: &LayoutBox,
    styles: &HashMap<dom::NodeId, style::ComputedStyle>,
) -> style::Clear {
    style_of(boxx, styles)
        .map(|cs| cs.clear)
        .unwrap_or(style::Clear::None)
}

/// The text alignment of a box's node (defaults to Left).
pub(crate) fn text_align_of(
    node: Option<dom::NodeId>,
    styles: &HashMap<dom::NodeId, style::ComputedStyle>,
) -> TextAlignLocal {
    match node.and_then(|n| styles.get(&n)).map(|cs| cs.text_align) {
        Some(style::TextAlign::Center) => TextAlignLocal::Center,
        Some(style::TextAlign::Right) => TextAlignLocal::Right,
        _ => TextAlignLocal::Left,
    }
}
