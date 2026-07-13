use crate::*;
use std::collections::HashMap;

// ---------------------------------------------------------------------------------------------
// Grid layout (basic explicit-track, row-major placement)
// ---------------------------------------------------------------------------------------------

/// Lay out the grid items of `boxx`. Resolves `grid-template-columns`/`rows` into pixel tracks,
/// places items row-major into cells (honoring explicit `grid-column`/`grid-row` start lines and
/// spans where parsed), applies gaps, and positions each item's content within its cell.
/// Unsupported: named areas, auto-flow `dense`, implicit-track sizing beyond an equal share.
pub(crate) fn layout_grid(
    boxx: &mut LayoutBox,
    ctx: Ctx,
    styles: &HashMap<dom::NodeId, style::ComputedStyle>,
    measurer: &dyn TextMeasurer,
) -> f32 {
    let cs = style_of(boxx, styles).cloned().unwrap_or_default();
    let content = boxx.dimensions.content;

    // In-flow item indices.
    let items: Vec<usize> = boxx
        .children
        .iter()
        .enumerate()
        .filter(|(_, c)| !is_out_of_flow(c, styles))
        .map(|(i, _)| i)
        .collect();

    // Column tracks: default to a single auto track if unspecified.
    let col_tracks = if cs.grid_template_columns.is_empty() {
        vec![style::TrackSize::Auto]
    } else {
        expand_auto_repeat_tracks(
            &cs.grid_template_columns,
            content.width,
            cs.column_gap,
            items.len(),
        )
    };
    let num_cols = col_tracks.len().max(1);

    // Determine number of rows: explicit rows, else enough to fit all items row-major.
    let explicit_rows = expand_auto_repeat_tracks(
        &cs.grid_template_rows,
        content.height,
        cs.row_gap,
        items.len(),
    );
    let num_rows = if !explicit_rows.is_empty() {
        explicit_rows.len()
    } else {
        items.len().div_ceil(num_cols).max(1)
    };

    // Resolve column widths in px.
    let col_widths = resolve_tracks(&col_tracks, content.width, cs.column_gap, num_cols);

    // Row heights: resolve explicit row tracks; for auto/implicit rows use a content estimate.
    let row_template = if explicit_rows.is_empty() {
        vec![style::TrackSize::Auto; num_rows]
    } else {
        // pad with Auto if fewer tracks than rows
        let mut v = explicit_rows.clone();
        while v.len() < num_rows {
            v.push(style::TrackSize::Auto);
        }
        v
    };

    // Place items into (row, col, col_span, row_span) cells, row-major.
    struct Placed {
        idx: usize,
        row: usize,
        col: usize,
        col_span: usize,
        row_span: usize,
    }
    let mut placed: Vec<Placed> = Vec::new();
    let mut cursor_r = 0usize;
    let mut cursor_c = 0usize;
    let mut occupied: std::collections::HashSet<(usize, usize)> = std::collections::HashSet::new();
    for &i in &items {
        let child = &boxx.children[i];
        let ccs = style_of(child, styles).cloned().unwrap_or_default();
        // Explicit placement?
        let (col, col_span) = placement_to_cell(ccs.grid_column, num_cols);
        let (row_opt, row_span) = match ccs.grid_row {
            Some(p) => placement_to_cell(Some(p), num_rows.max(1)),
            None => (None, 1),
        };

        let (final_r, final_c) = if let Some(c) = col {
            // explicit column; row explicit or current cursor row
            let r = row_opt.unwrap_or(cursor_r);
            (r, c)
        } else {
            // auto-place: find next free cell row-major
            let mut r = cursor_r;
            let mut c = cursor_c;
            loop {
                if c + col_span > num_cols {
                    r += 1;
                    c = 0;
                }
                if !occupied.contains(&(r, c)) {
                    break;
                }
                c += 1;
            }
            (r, c)
        };

        for dr in 0..row_span {
            for dc in 0..col_span {
                occupied.insert((final_r + dr, final_c + dc));
            }
        }
        // Advance auto cursor past this item.
        cursor_r = final_r;
        cursor_c = final_c + col_span;
        if cursor_c >= num_cols {
            cursor_c = 0;
            cursor_r += 1;
        }

        placed.push(Placed {
            idx: i,
            row: final_r,
            col: final_c,
            col_span,
            row_span,
        });
    }

    // Recompute number of rows actually used.
    let used_rows = placed
        .iter()
        .map(|p| p.row + p.row_span)
        .max()
        .unwrap_or(num_rows)
        .max(num_rows);
    let mut row_tmpl = row_template;
    while row_tmpl.len() < used_rows {
        row_tmpl.push(style::TrackSize::Auto);
    }

    // Column x offsets (cumulative + gaps).
    let mut col_x = vec![0.0f32; num_cols + 1];
    for c in 0..num_cols {
        col_x[c + 1] =
            col_x[c] + col_widths[c] + if c + 1 < num_cols { cs.column_gap } else { 0.0 };
    }

    // Measure each placed item's real content height by laying its contents out at its actual cell
    // width. The single-line `intrinsic_cross_height` estimate used for auto rows badly
    // underestimates wrapped paragraphs and nested flex/block subtrees, which would let the row
    // collapse and following grid rows / siblings overlap. We capture the laid-out height here and
    // feed it into the auto-row sizing below. (Items are laid out again at their final cell rect
    // after row heights are known, so this is purely a measurement pass.)
    let mut measured_h: Vec<f32> = vec![0.0; boxx.children.len()];
    // Per-item baseline offsets (first/last line baseline from the item's margin-box top) and
    // margin-box height — used for cross-axis (`align-items`) baseline alignment within cells.
    let mut first_bl: Vec<Option<f32>> = vec![None; boxx.children.len()];
    let mut last_bl: Vec<Option<f32>> = vec![None; boxx.children.len()];
    let mut mbox_h: Vec<f32> = vec![0.0; boxx.children.len()];
    for p in &placed {
        let end_col = (p.col + p.col_span).min(num_cols);
        let mut w = 0.0;
        for (k, cw) in col_widths[p.col..end_col].iter().enumerate() {
            w += cw;
            if p.col + k + 1 < end_col {
                w += cs.column_gap;
            }
        }
        let child = &mut boxx.children[p.idx];
        let m = child.dimensions.margin;
        let b = child.dimensions.border;
        let pad = child.dimensions.padding;
        let edges_h = m.left + m.right + b.left + b.right + pad.left + pad.right;
        let edges_v = m.top + m.bottom + b.top + b.bottom + pad.top + pad.bottom;
        let ccs = style_of(child, styles).cloned().unwrap_or_default();
        let cw = ccs.width.unwrap_or((w - edges_h).max(0.0));
        child.dimensions.content.width = cw;
        let laid = layout_flex_item_contents(child, ctx, styles, measurer);
        measured_h[p.idx] = laid;
        let mb_top = child.dimensions.margin_box().y;
        first_bl[p.idx] = crate::flex::nth_line_baseline(child, 1, styles).map(|x| x - mb_top);
        last_bl[p.idx] =
            crate::flex::nth_line_baseline(child, u32::MAX, styles).map(|x| x - mb_top);
        mbox_h[p.idx] = ccs.height.unwrap_or(laid) + edges_v;
    }

    // Row heights: resolve. For auto rows, use an equal share of explicit container height if
    // set, else the tallest measured item (margin-box) in that row.
    let container_h = explicit_height(boxx, styles);
    let placed_rows: Vec<(usize, usize)> = placed.iter().map(|p| (p.row, p.idx)).collect();
    let row_heights = resolve_row_heights(
        &row_tmpl,
        container_h,
        cs.row_gap,
        used_rows,
        &placed_rows,
        boxx,
        styles,
        measurer,
        &measured_h,
    );

    let mut row_y = vec![0.0f32; used_rows + 1];
    for r in 0..used_rows {
        row_y[r + 1] = row_y[r] + row_heights[r] + if r + 1 < used_rows { cs.row_gap } else { 0.0 };
    }

    boxx.grid_geometry = Some(GridGeometry {
        column_lines: match cs.direction {
            style::Direction::Ltr => col_x.iter().map(|x| content.x + *x).collect(),
            style::Direction::Rtl => col_x
                .iter()
                .map(|x| content.x + content.width - *x)
                .collect(),
        },
        row_lines: row_y.iter().map(|y| content.y + *y).collect(),
        direction: cs.direction,
    });

    // Cross-axis (`align-items`/`align-self`) baseline references per row: the deepest first-line
    // ascent for `baseline` groups, and the deepest descent (margin box below the last baseline) for
    // `last baseline` groups. Items in a group then share a baseline line within their cells.
    let resolved_align: Vec<style::AlignSelf> = boxx
        .children
        .iter()
        .map(|c| {
            match style_of(c, styles)
                .map(|s| s.align_self)
                .unwrap_or(style::AlignSelf::Auto)
            {
                style::AlignSelf::Auto => crate::flex::align_items_to_self(cs.align_items),
                other => other,
            }
        })
        .collect();
    let mut row_ascent = vec![f32::MIN; used_rows + 1];
    let mut row_descent = vec![f32::MIN; used_rows + 1];
    for p in &placed {
        let r = p.row.min(used_rows);
        match resolved_align[p.idx] {
            style::AlignSelf::Baseline => {
                if let Some(o) = first_bl[p.idx] {
                    row_ascent[r] = row_ascent[r].max(o);
                }
            }
            style::AlignSelf::LastBaseline => {
                if let Some(o) = last_bl[p.idx] {
                    row_descent[r] = row_descent[r].max(mbox_h[p.idx] - o);
                }
            }
            _ => {}
        }
    }

    for p in &placed {
        // Cell rect: from start column/row to span.
        let cell_x = content.x + col_x[p.col.min(num_cols)];
        let cell_y = content.y + row_y[p.row.min(used_rows)];
        let end_col = (p.col + p.col_span).min(num_cols);
        let end_row = (p.row + p.row_span).min(used_rows);
        // Width = sum of spanned column tracks + internal gaps.
        let mut w = 0.0;
        for (k, cw) in col_widths[p.col..end_col].iter().enumerate() {
            w += cw;
            if p.col + k + 1 < end_col {
                w += cs.column_gap;
            }
        }
        let mut h = 0.0;
        for (k, rh) in row_heights[p.row..end_row].iter().enumerate() {
            h += rh;
            if p.row + k + 1 < end_row {
                h += cs.row_gap;
            }
        }

        let child = &mut boxx.children[p.idx];
        let ccs = style_of(child, styles).cloned().unwrap_or_default();
        let m = child.dimensions.margin;
        let b = child.dimensions.border;
        let pad = child.dimensions.padding;
        let edges_h = m.left + m.right + b.left + b.right + pad.left + pad.right;
        let edges_v = m.top + m.bottom + b.top + b.bottom + pad.top + pad.bottom;
        let cw = ccs.width.unwrap_or((w - edges_h).max(0.0));
        let r = p.row.min(used_rows);
        // Cross-axis size + offset (margin-box top within the cell) per `align-items`/`align-self`.
        // Stretch fills the row; everything else uses the content height and is positioned.
        let (ch, cross_off) = match resolved_align[p.idx] {
            style::AlignSelf::Stretch => (ccs.height.unwrap_or((h - edges_v).max(0.0)), 0.0),
            align => {
                let item_ch = ccs.height.unwrap_or(measured_h[p.idx]);
                let item_mbh = item_ch + edges_v;
                let off = match align {
                    style::AlignSelf::FlexEnd => (h - item_mbh).max(0.0),
                    style::AlignSelf::Center => ((h - item_mbh) / 2.0).max(0.0),
                    style::AlignSelf::Baseline => first_bl[p.idx]
                        .filter(|_| row_ascent[r] > f32::MIN)
                        .map_or(0.0, |o| row_ascent[r] - o),
                    style::AlignSelf::LastBaseline => last_bl[p.idx]
                        .filter(|_| row_descent[r] > f32::MIN)
                        .map_or(0.0, |o| (h - row_descent[r]) - o),
                    _ => 0.0, // FlexStart / Auto
                };
                (item_ch, off)
            }
        };
        let cx = cell_x + m.left + b.left + pad.left;
        let cy = cell_y + cross_off + m.top + b.top + pad.top;
        child.dimensions.content = Rect {
            x: cx,
            y: cy,
            width: cw,
            height: ch,
        };
        layout_flex_item_contents(child, ctx, styles, measurer);
    }

    let total_h: f32 =
        row_heights.iter().sum::<f32>() + cs.row_gap * (used_rows.saturating_sub(1) as f32);
    container_h.unwrap_or(total_h)
}

fn expand_auto_repeat_tracks(
    tracks: &[style::TrackSize],
    available: f32,
    gap: f32,
    item_count: usize,
) -> Vec<style::TrackSize> {
    let mut expanded = Vec::new();
    for track in tracks {
        match *track {
            style::TrackSize::AutoRepeatFill(size) | style::TrackSize::AutoRepeatFit(size) => {
                let step = (size + gap).max(f32::EPSILON);
                let count = (((available + gap).max(0.0) / step).ceil() as usize).max(1);
                for index in 0..count {
                    let used = if matches!(track, style::TrackSize::AutoRepeatFit(_))
                        && index >= item_count
                    {
                        0.0
                    } else {
                        size
                    };
                    expanded.push(style::TrackSize::Px(used));
                }
            }
            other => expanded.push(other),
        }
    }
    expanded
}

/// Resolve a track list into pixel widths within `avail`, accounting for `gap` between tracks.
pub(crate) fn resolve_tracks(
    tracks: &[style::TrackSize],
    avail: f32,
    gap: f32,
    num: usize,
) -> Vec<f32> {
    let total_gap = gap * (num.saturating_sub(1) as f32);
    let space = (avail - total_gap).max(0.0);
    // Fixed (px + pct) consume space first; fr share the remainder; auto gets equal share of
    // whatever's left after fixed (treated like 1fr if any fr present, else equal split).
    let mut fixed = 0.0f32;
    let mut fr_total = 0.0f32;
    let mut auto_count = 0usize;
    for t in tracks {
        match t {
            style::TrackSize::Px(v) => fixed += v,
            style::TrackSize::Pct(p) => fixed += space * (p / 100.0),
            style::TrackSize::Fr(f) => fr_total += f,
            style::TrackSize::Auto => auto_count += 1,
            style::TrackSize::AutoRepeatFill(_) | style::TrackSize::AutoRepeatFit(_) => {}
        }
    }
    let remaining = (space - fixed).max(0.0);
    // Auto tracks take an equal share of remaining if no fr; if fr present, auto → 0 (min-content
    // approximated as 0 for simplicity).
    let auto_each = if fr_total == 0.0 && auto_count > 0 {
        remaining / auto_count as f32
    } else {
        0.0
    };
    let fr_unit = if fr_total > 0.0 {
        let after_auto = (remaining - auto_each * auto_count as f32).max(0.0);
        after_auto / fr_total
    } else {
        0.0
    };
    tracks
        .iter()
        .map(|t| match t {
            style::TrackSize::Px(v) => *v,
            style::TrackSize::Pct(p) => space * (p / 100.0),
            style::TrackSize::Fr(f) => fr_unit * f,
            style::TrackSize::Auto => auto_each,
            style::TrackSize::AutoRepeatFill(_) | style::TrackSize::AutoRepeatFit(_) => 0.0,
        })
        .collect()
}

/// Resolve row heights. Px/Pct/Fr resolve against the container height if known; auto rows get a
/// content estimate (max intrinsic item height in the row) or an equal share.
#[allow(clippy::too_many_arguments)]
pub(crate) fn resolve_row_heights(
    tracks: &[style::TrackSize],
    container_h: Option<f32>,
    gap: f32,
    num: usize,
    placed: &[(usize, usize)], // (row, child_index)
    boxx: &LayoutBox,
    styles: &HashMap<dom::NodeId, style::ComputedStyle>,
    measurer: &dyn TextMeasurer,
    measured_h: &[f32], // per-child laid-out content height (0 if not measured)
) -> Vec<f32> {
    // Content estimate per row: max item content height among items starting in that row. Use the
    // greater of the single-line intrinsic estimate and the actually-laid-out height (which
    // accounts for wrapped text and nested flex/grid/block subtrees), plus the item's own vertical
    // box edges, so the row track is tall enough to contain its items.
    let mut content_rows = vec![0.0f32; num];
    for &(r, idx) in placed {
        if r < num {
            let child = &boxx.children[idx];
            let est = intrinsic_cross_height(child, styles, measurer);
            let m = child.dimensions.margin;
            let b = child.dimensions.border;
            let p = child.dimensions.padding;
            let v_edges = m.top + m.bottom + b.top + b.bottom + p.top + p.bottom;
            let measured = measured_h.get(idx).copied().unwrap_or(0.0) + v_edges;
            let h = est.max(measured);
            content_rows[r] = content_rows[r].max(h);
        }
    }

    if let Some(ch) = container_h {
        // Resolve px/pct/fr against ch; auto → content estimate.
        let total_gap = gap * (num.saturating_sub(1) as f32);
        let space = (ch - total_gap).max(0.0);
        let mut fixed = 0.0f32;
        let mut fr_total = 0.0f32;
        for (i, t) in tracks.iter().enumerate() {
            match t {
                style::TrackSize::Px(v) => fixed += v,
                style::TrackSize::Pct(p) => fixed += space * (p / 100.0),
                style::TrackSize::Fr(f) => fr_total += f,
                style::TrackSize::Auto => fixed += content_rows[i],
                style::TrackSize::AutoRepeatFill(_) | style::TrackSize::AutoRepeatFit(_) => {}
            }
        }
        let remaining = (space - fixed).max(0.0);
        let fr_unit = if fr_total > 0.0 {
            remaining / fr_total
        } else {
            0.0
        };
        tracks
            .iter()
            .enumerate()
            .map(|(i, t)| match t {
                style::TrackSize::Px(v) => *v,
                style::TrackSize::Pct(p) => space * (p / 100.0),
                style::TrackSize::Fr(f) => fr_unit * f,
                style::TrackSize::Auto => content_rows[i],
                style::TrackSize::AutoRepeatFill(_) | style::TrackSize::AutoRepeatFit(_) => 0.0,
            })
            .collect()
    } else {
        // No container height: px/pct(→0) fixed, fr→content estimate, auto→content estimate.
        tracks
            .iter()
            .enumerate()
            .map(|(i, t)| match t {
                style::TrackSize::Px(v) => *v,
                style::TrackSize::Pct(_) => content_rows[i],
                style::TrackSize::Fr(_) => content_rows[i],
                style::TrackSize::Auto => content_rows[i],
                style::TrackSize::AutoRepeatFill(_) | style::TrackSize::AutoRepeatFit(_) => 0.0,
            })
            .collect()
    }
}

/// Convert a `GridPlacement` into a 0-based `(start_col, span)` within `num` tracks. Auto start
/// returns `(None-like)` via the column being unknown — callers handle `None` separately, so here
/// we encode auto-start as returning column 0 with the caller checking `start`. To keep the
/// signature simple this returns `(Option<usize>, span)`.
pub(crate) fn placement_to_cell(
    p: Option<style::GridPlacement>,
    num: usize,
) -> (Option<usize>, usize) {
    match p {
        None => (None, 1),
        Some(pl) => {
            let span = match pl.end {
                style::GridEnd::Span(s) => (s.max(1)) as usize,
                style::GridEnd::Line(e) => {
                    if let Some(s) = pl.start {
                        ((e - s).max(1)) as usize
                    } else if let Some(span) = pl.start_span {
                        span.max(1) as usize
                    } else {
                        1
                    }
                }
                style::GridEnd::Auto => 1,
            };
            let start = pl
                .start
                .or_else(|| match (pl.start_span, pl.end) {
                    (Some(span), style::GridEnd::Line(e)) => Some((e - span).max(1)),
                    _ => None,
                })
                .map(|s| {
                    // 1-based line → 0-based track index.
                    let idx = (s - 1).max(0) as usize;
                    idx.min(num.saturating_sub(1))
                });
            (start, span.min(num.max(1)))
        }
    }
}

fn line_pos(lines: &[f32], line: i32) -> Option<f32> {
    let idx = line.clamp(1, lines.len() as i32) as usize - 1;
    lines.get(idx).copied()
}

fn grid_axis_edges(
    placement: Option<style::GridPlacement>,
    lines: &[f32],
    start_edge: f32,
    end_edge: f32,
    start_sets_min: bool,
) -> (f32, f32) {
    let Some(pl) = placement else {
        return (start_edge.min(end_edge), start_edge.max(end_edge));
    };
    let start_line = pl
        .start
        .or_else(|| match (pl.start_span, pl.end) {
            (Some(span), style::GridEnd::Line(e)) => Some((e - span).max(1)),
            _ => None,
        })
        .and_then(|line| line_pos(lines, line));
    let end_line = match pl.end {
        style::GridEnd::Line(line) => line_pos(lines, line),
        style::GridEnd::Span(span) => pl
            .start
            .and_then(|start| line_pos(lines, start + span.max(1))),
        style::GridEnd::Auto => None,
    };

    let mut min_edge = start_edge.min(end_edge);
    let mut max_edge = start_edge.max(end_edge);
    if start_sets_min {
        if let Some(line) = start_line {
            min_edge = line;
        }
        if let Some(line) = end_line {
            max_edge = line;
        }
    } else {
        if let Some(line) = start_line {
            max_edge = line;
        }
        if let Some(line) = end_line {
            min_edge = line;
        }
    }
    (min_edge.min(max_edge), min_edge.max(max_edge))
}

pub(crate) fn abspos_grid_containing_block(
    parent: &LayoutBox,
    child: &LayoutBox,
    styles: &HashMap<dom::NodeId, style::ComputedStyle>,
) -> Option<Rect> {
    let geometry = parent.grid_geometry.as_ref()?;
    let cs = style_of(child, styles)?;
    let pad = parent.dimensions.padding_box();
    let (left, right) = grid_axis_edges(
        cs.grid_column,
        &geometry.column_lines,
        pad.x,
        pad.x + pad.width,
        geometry.direction == style::Direction::Ltr,
    );
    let (top, bottom) = grid_axis_edges(
        cs.grid_row,
        &geometry.row_lines,
        pad.y,
        pad.y + pad.height,
        true,
    );
    Some(Rect {
        x: left,
        y: top,
        width: (right - left).max(0.0),
        height: (bottom - top).max(0.0),
    })
}
