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
        cs.grid_template_columns.clone()
    };
    let num_cols = col_tracks.len().max(1);

    // Determine number of rows: explicit rows, else enough to fit all items row-major.
    let explicit_rows = cs.grid_template_rows.clone();
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
        let ccs = style_of(child, styles).cloned().unwrap_or_default();
        let cw = ccs.width.unwrap_or((w - edges_h).max(0.0));
        child.dimensions.content.width = cw;
        let laid = layout_flex_item_contents(child, ctx, styles, measurer);
        measured_h[p.idx] = laid;
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
        let ch = ccs.height.unwrap_or((h - edges_v).max(0.0));
        let cx = cell_x + m.left + b.left + pad.left;
        let cy = cell_y + m.top + b.top + pad.top;
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
                    } else {
                        1
                    }
                }
                style::GridEnd::Auto => 1,
            };
            let start = pl.start.map(|s| {
                // 1-based line → 0-based track index.
                let idx = (s - 1).max(0) as usize;
                idx.min(num.saturating_sub(1))
            });
            (start, span.min(num.max(1)))
        }
    }
}
