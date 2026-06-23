use crate::*;
use std::collections::HashMap;

// ---------------------------------------------------------------------------------------------
// Flexbox layout
// ---------------------------------------------------------------------------------------------

/// Lay out the flex items of `boxx` (a flex container whose content rect is already positioned
/// and width-sized). Returns the container's content height. Supports row/column (+ reverse),
/// wrap, gap, flex-grow/shrink/basis, justify-content, align-items/align-self.
pub(crate) fn layout_flex(
    boxx: &mut LayoutBox,
    ctx: Ctx,
    styles: &HashMap<dom::NodeId, style::ComputedStyle>,
    measurer: &dyn TextMeasurer,
) -> f32 {
    let cs = style_of(boxx, styles).cloned().unwrap_or_default();
    let content = boxx.dimensions.content;
    let is_row = matches!(
        cs.flex_direction,
        style::FlexDirection::Row | style::FlexDirection::RowReverse
    );
    let reverse = matches!(
        cs.flex_direction,
        style::FlexDirection::RowReverse | style::FlexDirection::ColumnReverse
    );
    let main_gap = if is_row { cs.column_gap } else { cs.row_gap };
    let cross_gap = if is_row { cs.row_gap } else { cs.column_gap };

    // The main-axis available size. For row this is content width; for column we use explicit
    // height if set, else a large value (single line) — content drives the height.
    let main_avail = if is_row {
        content.width
    } else {
        explicit_height(boxx, styles).unwrap_or(f32::INFINITY)
    };
    let cross_container = if is_row {
        explicit_height(boxx, styles) // None → derived from lines
    } else {
        Some(content.width)
    };

    // Collect in-flow item indices (skip out-of-flow), ordered by `order` then source order.
    let mut items: Vec<usize> = boxx
        .children
        .iter()
        .enumerate()
        .filter(|(_, c)| !is_out_of_flow(c, styles))
        .map(|(i, _)| i)
        .collect();
    items.sort_by_key(|&i| order_of(&boxx.children[i], styles));

    // Compute each item's base main size + cross size + flex factors.
    struct Item {
        idx: usize,
        hyp_main: f32, // base main size after clamping (no min/max here → == base)
        grow: f32,
        shrink: f32,
        cross: f32,
        // box-edge sums along each axis (margin+border+padding)
        main_edges: f32,
        cross_edges: f32,
        align: style::AlignSelf,
    }
    let mut metas: Vec<Item> = Vec::new();
    for &i in &items {
        let child = &boxx.children[i];
        let ccs = style_of(child, styles).cloned().unwrap_or_default();
        let m = child.dimensions.margin;
        let b = child.dimensions.border;
        let p = child.dimensions.padding;
        let (main_edges, cross_edges) = if is_row {
            (
                m.left + m.right + b.left + b.right + p.left + p.right,
                m.top + m.bottom + b.top + b.bottom + p.top + p.bottom,
            )
        } else {
            (
                m.top + m.bottom + b.top + b.bottom + p.top + p.bottom,
                m.left + m.right + b.left + b.right + p.left + p.right,
            )
        };
        // Base main size (content-box): flex-basis (px, then percentage of the container main size),
        // else explicit main size, else intrinsic.
        let base_content = if let Some(fb) = ccs.flex_basis {
            fb
        } else if let Some(pct) = ccs.flex_basis_pct.filter(|_| main_avail.is_finite()) {
            (main_avail * pct - main_edges).max(0.0)
        } else if is_row {
            ccs.width.unwrap_or_else(|| {
                (intrinsic_width(child, styles, measurer) - (p.left + p.right + b.left + b.right))
                    .max(0.0)
            })
        } else {
            ccs.height
                .unwrap_or_else(|| intrinsic_cross_height(child, styles, measurer))
        };
        let base_main = base_content + main_edges;
        // Cross base size (content-box) for the item.
        let cross_content = if is_row {
            ccs.height
                .unwrap_or_else(|| intrinsic_cross_height(child, styles, measurer))
        } else {
            ccs.width.unwrap_or_else(|| {
                (intrinsic_width(child, styles, measurer) - (p.left + p.right + b.left + b.right))
                    .max(0.0)
            })
        };
        metas.push(Item {
            idx: i,
            hyp_main: base_main,
            grow: ccs.flex_grow,
            shrink: ccs.flex_shrink,
            cross: cross_content + cross_edges,
            main_edges,
            cross_edges,
            align: ccs.align_self,
        });
    }

    // Break into flex lines (wrap if enabled and overflow).
    let wrap = !matches!(cs.flex_wrap, style::FlexWrap::NoWrap);
    let mut lines: Vec<Vec<usize>> = Vec::new(); // indices into `metas`
    if wrap && main_avail.is_finite() {
        let mut line: Vec<usize> = Vec::new();
        let mut used = 0.0f32;
        for (mi, m) in metas.iter().enumerate() {
            let add = if line.is_empty() {
                m.hyp_main
            } else {
                main_gap + m.hyp_main
            };
            if !line.is_empty() && used + add > main_avail {
                lines.push(std::mem::take(&mut line));
                used = m.hyp_main;
                line.push(mi);
            } else {
                used += add;
                line.push(mi);
            }
        }
        if !line.is_empty() {
            lines.push(line);
        }
    } else {
        lines.push((0..metas.len()).collect());
    }

    // Resolve each line: distribute free space, position along main & cross axes.
    let mut cross_cursor = if is_row { content.y } else { content.x };
    let mut line_cross_sizes: Vec<f32> = Vec::new();
    // First pass to know total cross used (for container sizing); we position as we go.
    let main_start = if is_row { content.x } else { content.y };

    // Determine final main container size for positioning.
    let main_box = if main_avail.is_finite() {
        main_avail
    } else {
        // single line, shrink-to-fit: sum of bases + gaps
        let sum: f32 = metas.iter().map(|m| m.hyp_main).sum::<f32>()
            + main_gap * (metas.len().saturating_sub(1) as f32);
        sum
    };

    for line in &lines {
        // Total base main size on this line.
        let n = line.len();
        let total_base: f32 = line.iter().map(|&mi| metas[mi].hyp_main).sum();
        let total_gap = main_gap * (n.saturating_sub(1) as f32);
        let free = main_box - total_base - total_gap;

        // Distribute free space: grow if positive, shrink if negative.
        let mut sizes: Vec<f32> = line.iter().map(|&mi| metas[mi].hyp_main).collect();
        if free > 0.0 {
            let total_grow: f32 = line.iter().map(|&mi| metas[mi].grow).sum();
            if total_grow > 0.0 {
                for (k, &mi) in line.iter().enumerate() {
                    sizes[k] += free * (metas[mi].grow / total_grow);
                }
            }
        } else if free < 0.0 {
            // Shrink is weighted by each item's *scaled* shrink factor (flex-shrink × base size), per
            // CSS Flexbox §9.7: a larger item gives up proportionally more space. Distributing the
            // deficit by raw flex-shrink alone over-shrinks small items to zero and leaves large ones
            // overflowing (e.g. Wikipedia's 55%/45% columns).
            let total_scaled: f32 = line
                .iter()
                .map(|&mi| metas[mi].shrink * metas[mi].hyp_main)
                .sum();
            if total_scaled > 0.0 {
                for (k, &mi) in line.iter().enumerate() {
                    let scaled = metas[mi].shrink * metas[mi].hyp_main;
                    sizes[k] += free * (scaled / total_scaled);
                    if sizes[k] < 0.0 {
                        sizes[k] = 0.0;
                    }
                }
            }
        }

        let used_main: f32 = sizes.iter().sum::<f32>() + total_gap;
        let leftover = (main_box - used_main).max(0.0);

        // justify-content: leading offset + spacing between items.
        let (mut pos, between_extra) = match cs.justify_content {
            style::JustifyContent::FlexStart => (0.0, 0.0),
            style::JustifyContent::FlexEnd => (leftover, 0.0),
            style::JustifyContent::Center => (leftover / 2.0, 0.0),
            style::JustifyContent::SpaceBetween => {
                if n > 1 {
                    (0.0, leftover / (n - 1) as f32)
                } else {
                    (0.0, 0.0)
                }
            }
            style::JustifyContent::SpaceAround => {
                let each = if n > 0 { leftover / n as f32 } else { 0.0 };
                (each / 2.0, each)
            }
            style::JustifyContent::SpaceEvenly => {
                let each = leftover / (n + 1) as f32;
                (each, each)
            }
        };

        // Order items along main axis (reverse handled by iterating in reverse).
        let order_iter: Vec<usize> = if reverse {
            line.iter().rev().cloned().collect()
        } else {
            line.to_vec()
        };
        // sizes are indexed by position in `line`; build a lookup.
        let size_of = |mi: usize| -> f32 {
            let k = line.iter().position(|&x| x == mi).unwrap();
            sizes[k]
        };

        // ---- Pass A: lay out each item's contents at its constrained main size so we learn the
        // height (row) / extent each item actually occupies. The intrinsic estimate in `metas`
        // is only a single line of text; wrapped content can be taller, and a nested
        // flex/grid/block child can be much taller. We use the real laid-out height to size the
        // line cross extent (row) and so the next item / line clears it. Without this, a flex
        // item that reported one line but wrapped to three would let the following sibling sit on
        // top of it (the observed vertical overlap).
        let mut actual_cross: Vec<f32> = vec![0.0; metas.len()]; // per-meta, item margin-box cross
        let mut actual_laid: Vec<f32> = vec![0.0; metas.len()]; // per-meta, content height laid out
        // For `align-*: [first|last] baseline`: each participating item's baseline as an offset from
        // its cross-start margin edge. Only well-defined for a (horizontal) row container — see below.
        let mut item_baseline: Vec<Option<f32>> = vec![None; metas.len()];
        for &mi in line {
            let item_main = size_of(mi);
            let meta = &metas[mi];
            let child = &mut boxx.children[meta.idx];
            // Tentatively size the content box so contents lay out at the right main extent.
            let content_main = (item_main - meta.main_edges).max(0.0);
            if is_row {
                child.dimensions.content.width = content_main;
                child.dimensions.content.height = (meta.cross - meta.cross_edges).max(0.0);
            } else {
                child.dimensions.content.height = content_main;
                child.dimensions.content.width = (meta.cross - meta.cross_edges).max(0.0);
            }
            let laid = layout_flex_item_contents(child, ctx, styles, measurer);
            actual_laid[mi] = laid;
            // The item's cross-axis margin-box extent: explicit cross size if set, else the
            // greater of the single-line estimate and what the contents actually needed.
            let has_explicit_cross = if is_row {
                explicit_height(child, styles).is_some()
            } else {
                explicit_width(child, styles).is_some()
            };
            let cross_extent = if is_row {
                if has_explicit_cross {
                    meta.cross
                } else {
                    (laid + meta.cross_edges).max(meta.cross)
                }
            } else {
                // column: cross is width — content layout doesn't change width, keep estimate.
                meta.cross
            };
            actual_cross[mi] = cross_extent;

            // Record the item's baseline if it participates in baseline alignment. Only meaningful
            // when the item's baseline is parallel to the cross axis — for a ROW container that's the
            // usual case; for a COLUMN container the baseline is perpendicular to the cross axis and
            // the spec falls back to start alignment, so we leave it `None`.
            let resolved = match metas[mi].align {
                style::AlignSelf::Auto => align_items_to_self(cs.align_items),
                other => other,
            };
            if is_row
                && matches!(
                    resolved,
                    style::AlignSelf::Baseline | style::AlignSelf::LastBaseline
                )
            {
                let last = matches!(resolved, style::AlignSelf::LastBaseline);
                item_baseline[mi] =
                    Some(flex_item_baseline(&boxx.children[metas[mi].idx], last, styles));
            }
        }

        // The line's reference baseline = the largest item baseline; baseline-aligned items then
        // shift so their baselines coincide there.
        let ref_baseline: Option<f32> = item_baseline
            .iter()
            .filter_map(|b| *b)
            .fold(None, |acc, b| Some(acc.map_or(b, |a: f32| a.max(b))));

        // Cross size of this line = max actual item cross size, but if the container has an
        // explicit cross size and this is a single line, the line fills the container's cross
        // extent (so alignment / stretch are measured against the container box).
        let content_cross: f32 = line.iter().map(|&mi| actual_cross[mi]).fold(0.0, f32::max);
        let line_cross: f32 = match cross_container {
            Some(c) if lines.len() == 1 => c.max(content_cross),
            _ => content_cross,
        };

        // ---- Pass B: position each item now that the line cross size is known. For column flex
        // the per-item main extent grows to the laid-out height too, so items stack without
        // overlap; track the running main `pos` from the actual sizes.
        let mut col_pos = pos; // running main position for column (uses actual main extents)
        for &mi in &order_iter {
            let item_main = size_of(mi);
            let meta = &metas[mi];
            // Cross placement within the line.
            let align = match meta.align {
                style::AlignSelf::Auto => align_items_to_self(cs.align_items),
                other => other,
            };
            let this_cross = actual_cross[mi];
            let item_cross_outer = match align {
                style::AlignSelf::Stretch => line_cross,
                _ => this_cross,
            };
            let cross_off = match align {
                style::AlignSelf::FlexStart | style::AlignSelf::Stretch | style::AlignSelf::Auto => {
                    0.0
                }
                style::AlignSelf::FlexEnd => line_cross - this_cross,
                style::AlignSelf::Center => (line_cross - this_cross) / 2.0,
                // Baseline: shift the item down so its baseline meets the line's reference baseline.
                // Falls back to start when this item has no cross-axis baseline (e.g. a column
                // container — see Pass A, where `item_baseline` stays `None`).
                style::AlignSelf::Baseline | style::AlignSelf::LastBaseline => {
                    match (ref_baseline, item_baseline[mi]) {
                        (Some(r), Some(b)) => (r - b).max(0.0),
                        _ => 0.0,
                    }
                }
            };
            let cross_off = if matches!(align, style::AlignSelf::Stretch) {
                0.0
            } else {
                cross_off
            };

            // For column flex the main extent is the laid-out height (so the next item clears it).
            let main_extent = if is_row {
                item_main
            } else {
                let has_explicit_main = explicit_height(&boxx.children[meta.idx], styles).is_some();
                if has_explicit_main {
                    item_main
                } else {
                    let laid_main = actual_laid[mi] + meta.main_edges;
                    laid_main.max(item_main)
                }
            };

            // Compute the child's margin-box origin in (main, cross) then map to (x, y).
            let cur_main = if is_row { pos } else { col_pos };
            let main_origin = main_start + cur_main;
            let cross_origin = cross_cursor + cross_off;

            // Place the child. content size along main = item_main - main_edges; along cross =
            // item_cross_outer - cross_edges.
            let child = &mut boxx.children[meta.idx];
            let m = child.dimensions.margin;
            let b = child.dimensions.border;
            let p = child.dimensions.padding;
            let content_main = (item_main - meta.main_edges).max(0.0);
            let content_cross = (item_cross_outer - meta.cross_edges).max(0.0);

            let (cx, cy, cw, ch) = if is_row {
                (
                    main_origin + m.left + b.left + p.left,
                    cross_origin + m.top + b.top + p.top,
                    content_main,
                    content_cross,
                )
            } else {
                (
                    cross_origin + m.left + b.left + p.left,
                    main_origin + m.top + b.top + p.top,
                    content_cross,
                    (main_extent - meta.main_edges).max(0.0),
                )
            };
            child.dimensions.content = Rect {
                x: cx,
                y: cy,
                width: cw,
                height: ch,
            };

            // Re-lay out contents at the final position so descendant boxes are correctly placed.
            layout_flex_item_contents(child, ctx, styles, measurer);

            if is_row {
                pos += item_main + main_gap + between_extra;
            } else {
                col_pos += main_extent + main_gap + between_extra;
            }
        }
        if !is_row {
            pos = col_pos;
        }
        let _ = pos;

        line_cross_sizes.push(line_cross);
        cross_cursor += line_cross + cross_gap;
    }

    // Container cross size = explicit, else sum of line cross sizes + gaps.
    let total_cross: f32 = line_cross_sizes.iter().sum::<f32>()
        + cross_gap * (line_cross_sizes.len().saturating_sub(1) as f32);
    if is_row {
        explicit_height(boxx, styles).unwrap_or(total_cross)
    } else {
        // column: height is the main size used.
        let used = line_cross_sizes; // not used here
        let _ = used;
        // main extent = bottom-most item; recompute from positions:
        let mut max_bottom = content.y;
        for c in &boxx.children {
            if !is_out_of_flow(c, styles) {
                max_bottom =
                    max_bottom.max(c.dimensions.margin_box().y + c.dimensions.margin_box().height);
            }
        }
        explicit_height(boxx, styles).unwrap_or((max_bottom - content.y).max(0.0))
    }
}

/// The `order` of a flex item.
pub(crate) fn order_of(
    boxx: &LayoutBox,
    styles: &HashMap<dom::NodeId, style::ComputedStyle>,
) -> i32 {
    style_of(boxx, styles).map(|cs| cs.order).unwrap_or(0)
}

/// The first (or `last`) baseline of a laid-out flex item, as an offset DOWN from the item's
/// margin-box top edge (for a row container). Per CSS Flexbox §8.3 the baseline propagates from the
/// item's startmost/endmost in-flow descendant: we descend the first (or last) in-flow child until a
/// leaf, then take a text leaf's alphabetic baseline (≈ 0.8·font-size below its top, matching the
/// painter) or, for an atomic leaf (inline-block / replaced / empty box), its bottom margin edge —
/// which is the synthesized baseline of a box with no in-flow line. This yields the exact values the
/// flex-baseline reftests expect (e.g. an `inline-block; height:1em` span contributes a `1em`
/// baseline, and a nested flex container contributes its first/last item's baseline).
fn flex_item_baseline(
    item: &LayoutBox,
    last: bool,
    styles: &HashMap<dom::NodeId, style::ComputedStyle>,
) -> f32 {
    fn leaf_baseline_abs(
        b: &LayoutBox,
        last: bool,
        styles: &HashMap<dom::NodeId, style::ComputedStyle>,
    ) -> f32 {
        if let BoxContent::Text(_) | BoxContent::Marker(_) = &b.content {
            return b.dimensions.content.y + b.style.font_size * 0.8;
        }
        // Pick the startmost child for a first baseline (endmost for a last baseline). A
        // *-reverse flex container lays its items out against source order, so flip the choice
        // there — that's what makes the `row-reverse`/`column-reverse` baselines come out right.
        let reverse = matches!(
            style_of(b, styles).map(|cs| cs.flex_direction),
            Some(style::FlexDirection::RowReverse | style::FlexDirection::ColumnReverse)
        );
        let pick_last = last ^ reverse;
        // A fieldset `<legend>` is laid out in the border and doesn't contribute to the baseline.
        let next = if pick_last {
            b.children.iter().rev().find(|c| !c.style.is_legend)
        } else {
            b.children.iter().find(|c| !c.style.is_legend)
        };
        match next {
            // A childless box is the leaf (atomic / empty), whose synthesized baseline is its bottom
            // margin edge.
            Some(c) => leaf_baseline_abs(c, last, styles),
            None => b.dimensions.margin_box().y + b.dimensions.margin_box().height,
        }
    }
    let top = item.dimensions.margin_box().y;
    leaf_baseline_abs(item, last, styles) - top
}

/// Map a container's `align-items` to the equivalent per-item `align-self`.
pub(crate) fn align_items_to_self(a: style::AlignItems) -> style::AlignSelf {
    match a {
        style::AlignItems::Stretch => style::AlignSelf::Stretch,
        style::AlignItems::FlexStart => style::AlignSelf::FlexStart,
        style::AlignItems::FlexEnd => style::AlignSelf::FlexEnd,
        style::AlignItems::Center => style::AlignSelf::Center,
        style::AlignItems::Baseline => style::AlignSelf::Baseline,
        style::AlignItems::LastBaseline => style::AlignSelf::LastBaseline,
    }
}

/// An intrinsic cross-axis height estimate for a flex item (single line of text or explicit).
pub(crate) fn intrinsic_cross_height(
    boxx: &LayoutBox,
    styles: &HashMap<dom::NodeId, style::ComputedStyle>,
    measurer: &dyn TextMeasurer,
) -> f32 {
    if let Some(h) = explicit_height(boxx, styles) {
        return h;
    }
    // One line of text at the box's font size, or 0.
    let fs = boxx.style.font_size;
    let has_text = has_any_text(boxx);
    if has_text {
        measurer.line_height(
            if fs > 0.0 { fs } else { 16.0 },
            boxx.style.font_family.as_deref(),
        )
    } else {
        0.0
    }
}

pub(crate) fn has_any_text(boxx: &LayoutBox) -> bool {
    if matches!(&boxx.content, BoxContent::Text(_)) {
        return true;
    }
    boxx.children.iter().any(has_any_text)
}

/// Lay out a flex item's own contents now that its content rect is fixed. The item itself acts
/// like a block container for its children. Returns the height the item's content actually
/// occupied once laid out at its constrained width (the intrinsic estimate used to size the
/// flex line is only an approximation; text can wrap to more lines than estimated). Callers use
/// this to grow the item / container so following items and siblings don't overlap.
pub(crate) fn layout_flex_item_contents(
    boxx: &mut LayoutBox,
    ctx: Ctx,
    styles: &HashMap<dom::NodeId, style::ComputedStyle>,
    measurer: &dyn TextMeasurer,
) -> f32 {
    let child_ctx = if !matches!(position_of(boxx, styles), style::Position::Static) {
        Ctx {
            positioned: boxx.dimensions.padding_box(),
            viewport: ctx.viewport,
        }
    } else {
        ctx
    };
    let display = display_of(boxx, styles);
    let laid_out = match display {
        style::Display::Flex | style::Display::InlineFlex => {
            layout_flex(boxx, child_ctx, styles, measurer)
        }
        style::Display::Grid | style::Display::InlineGrid => {
            layout_grid(boxx, child_ctx, styles, measurer)
        }
        _ => {
            let any_block = boxx.children.iter().any(|c| {
                matches!(c.content, BoxContent::Block | BoxContent::Anonymous)
                    || (matches!(c.content, BoxContent::Image(_) | BoxContent::Widget(_))
                        && image_is_block(c, styles))
            });
            if any_block {
                layout_block_children(boxx, child_ctx, styles, measurer)
            } else if !boxx.children.is_empty() {
                let align = text_align_of(boxx.node, styles);
                let indent = text_indent_of(boxx.node, styles);
                layout_inline_children(boxx, align, indent, child_ctx, styles, measurer)
            } else {
                0.0
            }
        }
    };
    resolve_out_of_flow(boxx, child_ctx, styles, measurer);
    laid_out
}
