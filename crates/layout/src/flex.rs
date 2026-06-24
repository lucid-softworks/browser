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
    // The flex main axis is physically horizontal only when the container's writing mode agrees with
    // the flex-direction: a `row` in horizontal-tb, or a `column` in a vertical writing mode. So in a
    // vertical writing mode the axes swap. (`^` since exactly one of the two flips the axis.)
    let vertical_wm = matches!(
        cs.writing_mode,
        style::WritingMode::VerticalRl | style::WritingMode::VerticalLr
    );
    let main_horizontal = is_row ^ vertical_wm;
    let main_gap = if main_horizontal { cs.column_gap } else { cs.row_gap };
    let cross_gap = if main_horizontal { cs.row_gap } else { cs.column_gap };

    // The main-axis available size. For row this is content width; for column we use explicit
    // height if set, else a large value (single line) — content drives the height.
    let main_avail = if main_horizontal {
        content.width
    } else {
        explicit_height(boxx, styles).unwrap_or(f32::INFINITY)
    };
    let cross_container = if main_horizontal {
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
        let (main_edges, cross_edges) = if main_horizontal {
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
        } else if main_horizontal {
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
        let cross_content = if main_horizontal {
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
    let mut cross_cursor = if main_horizontal { content.y } else { content.x };
    let mut line_cross_sizes: Vec<f32> = Vec::new();
    // First pass to know total cross used (for container sizing); we position as we go.
    let main_start = if main_horizontal { content.x } else { content.y };

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
            if main_horizontal {
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
            let has_explicit_cross = if main_horizontal {
                explicit_height(child, styles).is_some()
            } else {
                explicit_width(child, styles).is_some()
            };
            let cross_extent = if has_explicit_cross {
                meta.cross
            } else if main_horizontal {
                (laid + meta.cross_edges).max(meta.cross)
            } else {
                // Vertical container: cross is the width. The intrinsic estimate (`meta.cross`) misses
                // inline-block / atomic content (`intrinsic_width` only counts text + block children),
                // so fall back to the actually laid-out margin-box width.
                child.dimensions.margin_box().width.max(meta.cross)
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
            if main_horizontal
                && matches!(
                    resolved,
                    style::AlignSelf::Baseline | style::AlignSelf::LastBaseline
                )
            {
                let last = matches!(resolved, style::AlignSelf::LastBaseline);
                item_baseline[mi] =
                    Some(flex_item_baseline(&boxx.children[metas[mi].idx], last, vertical_wm, styles));
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
            let main_extent = if main_horizontal {
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
            let cur_main = if main_horizontal { pos } else { col_pos };
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

            let (cx, cy, cw, ch) = if main_horizontal {
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

            if main_horizontal {
                pos += item_main + main_gap + between_extra;
            } else {
                col_pos += main_extent + main_gap + between_extra;
            }
        }
        if !main_horizontal {
            pos = col_pos;
        }
        let _ = pos;

        line_cross_sizes.push(line_cross);
        cross_cursor += line_cross + cross_gap;
    }

    // Container cross size = explicit, else sum of line cross sizes + gaps.
    let total_cross: f32 = line_cross_sizes.iter().sum::<f32>()
        + cross_gap * (line_cross_sizes.len().saturating_sub(1) as f32);
    if main_horizontal {
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
    container_vertical: bool,
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
        // A fieldset `<legend>` is laid out in the border, and a table `<caption>` is outside the
        // table grid — neither contributes to the box's baseline (a table's baseline is its rows').
        let skip = |c: &&LayoutBox| -> bool {
            c.style.is_legend
                || matches!(
                    style_of(c, styles).map(|s| s.display),
                    Some(style::Display::TableCaption)
                )
        };
        let next = if pick_last {
            b.children.iter().rev().find(|c| !skip(c))
        } else {
            b.children.iter().find(|c| !skip(c))
        };
        match next {
            // A childless box is the leaf (atomic / empty), whose synthesized baseline is its bottom
            // margin edge.
            Some(c) => leaf_baseline_abs(c, last, styles),
            None => b.dimensions.margin_box().y + b.dimensions.margin_box().height,
        }
    }
    // A vertical-writing-mode item's alphabetic baseline runs horizontally (perpendicular to a
    // vertical cross axis), so it can't be propagated as-is. Two cases:
    //   * orthogonal to the container (horizontal-tb container): synthesize from the margin box — the
    //     alphabetic baseline sits at the margin-box end (bottom) edge.
    //   * parallel to the container (vertical container, so the cross axis is the item's own inline
    //     axis): use the central baseline — the middle of the margin box.
    if matches!(
        style_of(item, styles).map(|cs| cs.writing_mode),
        Some(style::WritingMode::VerticalRl | style::WritingMode::VerticalLr)
    ) {
        let h = item.dimensions.margin_box().height;
        return if container_vertical { h / 2.0 } else { h };
    }
    let top = item.dimensions.margin_box().y;
    // A table's baseline is its first (or last) row's baseline: among the cells of that row (grouped
    // by their top edge), the deepest first-line baseline. Captions are excluded.
    if matches!(
        style_of(item, styles).map(|cs| cs.display),
        Some(style::Display::Table)
    ) {
        if let Some(b) = table_baseline(item, last, styles) {
            return b - top;
        }
    }
    // `-webkit-line-clamp: N` truncates the box to N lines, so its *last* baseline is the Nth line's
    // baseline, not its true final line's.
    if last {
        if let Some(n) = style_of(item, styles).and_then(|cs| cs.line_clamp) {
            if let Some(b) = nth_line_baseline(item, n, styles) {
                return b - top;
            }
        }
        // A multi-column box's last baseline is its lowest line across all columns (the bottom of the
        // tallest column), not its source-last child — `nth_line_baseline` with a huge N returns that.
        if style_of(item, styles).is_some_and(|cs| cs.column_count.is_some()) {
            if let Some(b) = nth_line_baseline(item, u32::MAX, styles) {
                return b - top;
            }
        }
    }
    let mut abs = leaf_baseline_abs(item, last, styles);
    // A scroll container (overflow != visible) clamps its propagated baseline to its own border box:
    // content scrolled or pushed (e.g. a negative margin) out of the scrollport doesn't drag the
    // baseline outside the box — it pins to the near border edge instead (CSS scroll-container
    // baselines).
    // Only when the height is constrained (explicit height/block-size) — otherwise the box grows to
    // fit its content, so the baseline is already inside it and the border box can't be trusted (e.g.
    // a `-webkit-line-clamp` box, whose used height the engine doesn't compute).
    if item.style.clips_overflow
        && style_of(item, styles).is_some_and(|cs| cs.height.is_some())
    {
        let bb = item.dimensions.border_box();
        abs = abs.clamp(bb.y, bb.y + bb.height);
    }
    abs - top
}

/// The absolute (document-space) baseline of the Nth line of `item` (1-indexed, clamped to the line
/// count) — used for a `-webkit-line-clamp` box's last baseline. Lines are split by forced breaks;
/// each line's baseline is the lowest of its leaves' baselines (text alphabetic, else the leaf's
/// bottom margin edge). `None` if the box has no leaves.
/// The absolute baseline of a table's first (or `last`) row: the cells are grouped into rows by their
/// top edge, and the chosen row's baseline is the deepest first-line baseline among its cells (cells
/// with `vertical-align: baseline` share that line). Captions are excluded. `None` if no cells.
fn table_baseline(
    item: &LayoutBox,
    last: bool,
    styles: &HashMap<dom::NodeId, style::ComputedStyle>,
) -> Option<f32> {
    fn collect(
        b: &LayoutBox,
        styles: &HashMap<dom::NodeId, style::ComputedStyle>,
        out: &mut Vec<(f32, f32)>,
    ) {
        match style_of(b, styles).map(|cs| cs.display) {
            Some(style::Display::TableCaption) => {}
            Some(style::Display::TableCell) => {
                // First-line baseline if the cell has content; otherwise the content-box bottom (the
                // synthesized baseline of an empty cell).
                let cbottom = b.dimensions.content.y + b.dimensions.content.height;
                let bl = if b.children.is_empty() {
                    cbottom
                } else {
                    nth_line_baseline(b, 1, styles).unwrap_or(cbottom)
                };
                // Key on the cell's border-box top (the row position) — cells in one row share it even
                // when their differing borders give their content boxes different tops.
                out.push((b.dimensions.border_box().y, bl));
            }
            _ => {
                for c in &b.children {
                    collect(c, styles, out);
                }
            }
        }
    }
    let mut cells: Vec<(f32, f32)> = Vec::new();
    collect(item, styles, &mut cells);
    let row_top = if last {
        cells.iter().map(|c| c.0).fold(f32::MIN, f32::max)
    } else {
        cells.iter().map(|c| c.0).fold(f32::MAX, f32::min)
    };
    cells
        .iter()
        .filter(|c| (c.0 - row_top).abs() < 0.5)
        .map(|c| c.1)
        .fold(None, |acc, b| Some(acc.map_or(b, |a: f32| a.max(b))))
}

fn nth_line_baseline(
    item: &LayoutBox,
    n: u32,
    styles: &HashMap<dom::NodeId, style::ComputedStyle>,
) -> Option<f32> {
    // Collect each leaf as `(top, baseline)`. A `-webkit-box` line-clamp box stacks its lines as flex
    // items / block children (distinct `y`), while a plain block stacks them as inline lines — both
    // give one leaf-group per visual line when grouped by top edge, so we don't rely on line breaks.
    // `<caption>` subtrees are skipped (they're outside a table's grid / baseline).
    fn walk(
        b: &LayoutBox,
        styles: &HashMap<dom::NodeId, style::ComputedStyle>,
        out: &mut Vec<(f32, f32)>,
    ) {
        if matches!(
            style_of(b, styles).map(|cs| cs.display),
            Some(style::Display::TableCaption)
        ) {
            return;
        }
        match &b.content {
            BoxContent::LineBreak => {}
            BoxContent::Text(_) | BoxContent::Marker(_) => {
                out.push((
                    b.dimensions.content.y,
                    b.dimensions.content.y + b.style.font_size * 0.8,
                ));
            }
            _ if b.children.is_empty() => {
                let mb = b.dimensions.margin_box();
                out.push((mb.y, mb.y + mb.height));
            }
            _ => {
                for c in &b.children {
                    walk(c, styles, out);
                }
            }
        }
    }
    let mut leaves: Vec<(f32, f32)> = Vec::new();
    walk(item, styles, &mut leaves);
    if leaves.is_empty() {
        return None;
    }
    // Group leaves into visual lines by top edge (sorted top-to-bottom); each line's baseline is the
    // lowest of its leaves'.
    leaves.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
    let mut lines: Vec<f32> = Vec::new();
    let mut cur_top = f32::MIN;
    for (top, bl) in leaves {
        if lines.is_empty() || (top - cur_top).abs() > 0.5 {
            lines.push(bl);
            cur_top = top;
        } else {
            let last = lines.last_mut().expect("a line");
            *last = last.max(bl);
        }
    }
    let idx = (n as usize).min(lines.len()).saturating_sub(1);
    Some(lines[idx])
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
    // Vertical writing mode: the physical height (a row flex item's cross extent) is the INLINE size
    // — the longest line's inline extent (inline-block / replaced heights summed per line, lines split
    // by forced breaks) — not a single horizontal line-height.
    if matches!(
        style_of(boxx, styles).map(|cs| cs.writing_mode),
        Some(style::WritingMode::VerticalRl | style::WritingMode::VerticalLr)
    ) {
        return vertical_inline_extent(boxx, styles);
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

/// The inline size (physical height) of a vertical-writing-mode box: the longest line's inline
/// extent, where each line (split by forced break) sums the inline-axis extent of its atomic
/// inline-level boxes (inline-block / replaced → their margin-box height). Mirror of
/// `intrinsic::vertical_block_size` for the perpendicular (inline) axis.
fn vertical_inline_extent(
    boxx: &LayoutBox,
    styles: &HashMap<dom::NodeId, style::ComputedStyle>,
) -> f32 {
    fn walk(
        children: &[LayoutBox],
        styles: &HashMap<dom::NodeId, style::ComputedStyle>,
        lines: &mut Vec<f32>,
    ) {
        for c in children {
            match &c.content {
                BoxContent::LineBreak => lines.push(0.0),
                BoxContent::Image(_) | BoxContent::Widget(_) => {
                    *lines.last_mut().expect("a line") += c.dimensions.margin_box().height;
                }
                _ => {
                    if let Some(h) = explicit_height(c, styles) {
                        let m = c.dimensions.margin;
                        let b = c.dimensions.border;
                        let p = c.dimensions.padding;
                        *lines.last_mut().expect("a line") +=
                            h + m.top + m.bottom + b.top + b.bottom + p.top + p.bottom;
                    } else {
                        walk(&c.children, styles, lines);
                    }
                }
            }
        }
    }
    let mut lines = vec![0.0f32];
    walk(&boxx.children, styles, &mut lines);
    lines.into_iter().fold(0.0, f32::max)
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
        style::Display::Table => crate::table::layout_table(boxx, child_ctx, styles, measurer),
        _ if style_of(boxx, styles).and_then(|cs| cs.column_count).is_some() => {
            crate::block::layout_multicol(boxx, child_ctx, styles, measurer)
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
