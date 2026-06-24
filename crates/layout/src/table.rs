use crate::*;
use std::collections::HashMap;

// ---------------------------------------------------------------------------------------------
// Table layout (table formatting context)
// ---------------------------------------------------------------------------------------------

/// One table cell, lifted out of the table subtree for grid placement. `boxx` is the cell's own
/// `LayoutBox` (a `display: table-cell` box, with its content children); `col`/`row` are its
/// 0-based grid position; `colspan`/`rowspan` how many columns/rows it covers.
pub(crate) struct TableCell {
    boxx: LayoutBox,
    col: usize,
    row: usize,
    colspan: usize,
    rowspan: usize,
}

// Per-layout snapshot of every table cell's `colspan`/`rowspan` (keyed by the cell's DOM node id).
// Populated by [`layout_document`] from the DOM (where the attributes live) and read by
// [`layout_table`], which only has the styles map and the box tree — not the document. A
// thread-local keeps the spans out of the hot `LayoutBox`/`BoxContent` types (which a prior agent
// warned must stay small for deep-nesting stack safety) without threading a new parameter through
// every layout function.
thread_local! {
    static TABLE_SPANS: std::cell::RefCell<HashMap<dom::NodeId, (usize, usize)>> =
        std::cell::RefCell::new(HashMap::new());
}

/// Scan the whole document for `<td>`/`<th>` cells (or any element with a `colspan`/`rowspan`
/// attribute) and record their `(colspan, rowspan)` into the thread-local span snapshot. Values are
/// clamped to `[1, 1000]` so a hostile `colspan=100000000` can't blow up the column model.
pub(crate) fn capture_table_spans(doc: &dom::Document) {
    fn parse_span(el: &dom::ElementData, name: &str) -> usize {
        el.attrs
            .get(name)
            .and_then(|v| v.trim().parse::<usize>().ok())
            .unwrap_or(1)
            .clamp(1, 1000)
    }
    TABLE_SPANS.with(|t| {
        let mut map = t.borrow_mut();
        map.clear();
        for id in (0..doc.len()).map(dom::NodeId) {
            if let dom::NodeData::Element(el) = &doc.get(id).data {
                let tag = el.tag.to_ascii_lowercase();
                // `<col>`/`<colgroup>` use the `span` attribute; cells use `colspan`. Store both in
                // the colspan channel so `col_span_attr`/`table_span` can read them uniformly.
                let cs = if tag == "col" || tag == "colgroup" {
                    parse_span(el, "span")
                } else {
                    parse_span(el, "colspan")
                };
                let rs = parse_span(el, "rowspan");
                if cs != 1 || rs != 1 {
                    map.insert(id, (cs, rs));
                }
            }
        }
    });
}

/// The `colspan` (`name == "colspan"`) or `rowspan` of a cell box, read from the thread-local span
/// snapshot (default 1 when the cell has no such attribute).
pub(crate) fn table_span(cell: &LayoutBox, name: &str) -> usize {
    let node = match cell.node {
        Some(n) => n,
        None => return 1,
    };
    TABLE_SPANS.with(|t| {
        t.borrow()
            .get(&node)
            .map(|(c, r)| if name == "colspan" { *c } else { *r })
            .unwrap_or(1)
    })
}

/// Gather the table's structure: descend through row-group wrappers (`thead`/`tbody`/`tfoot`,
/// recognized by `display`) and direct `<tr>` rows, in the spec's visual order (header groups
/// first, then body groups + direct rows in document order, then footer groups). Each returned
/// element is a list of cell `LayoutBox`es (the `display: table-cell` children of one `<tr>`).
/// Caption boxes are pulled out separately by the caller.
pub(crate) fn collect_table_rows(
    table: &mut LayoutBox,
    styles: &HashMap<dom::NodeId, style::ComputedStyle>,
) -> Vec<Vec<LayoutBox>> {
    // Split the table's direct children into header groups / body content / footer groups.
    let mut headers: Vec<Vec<LayoutBox>> = Vec::new();
    let mut bodies: Vec<Vec<LayoutBox>> = Vec::new();
    let mut footers: Vec<Vec<LayoutBox>> = Vec::new();

    // Drain the table's children so we can move cells out by value.
    let children = std::mem::take(&mut table.children);
    for child in children {
        let d = style_of(&child, styles).map(|cs| cs.display);
        match d {
            Some(style::Display::TableRow) => {
                bodies.push(extract_cells(child, styles));
            }
            Some(style::Display::TableHeaderGroup) => {
                collect_group_rows(child, styles, &mut headers);
            }
            Some(style::Display::TableFooterGroup) => {
                collect_group_rows(child, styles, &mut footers);
            }
            Some(style::Display::TableRowGroup) => {
                collect_group_rows(child, styles, &mut bodies);
            }
            // Caption / column(-group) / stray content: ignored here (captions handled separately,
            // columns don't produce rows). Anonymous or text boxes directly under a table are
            // dropped (they have no place in the row/cell grid).
            _ => {}
        }
    }

    let mut rows = headers;
    rows.append(&mut bodies);
    rows.append(&mut footers);
    rows
}

/// Collect explicit per-column widths declared by `<colgroup>`/`<col>` children of the table, in
/// column order. Each entry is `Some(px)` when a column has an explicit width (from a `width`
/// attribute mapped to `style.width`, or CSS `width` on the `<col>`), else `None`. A `<col span=N>`
/// repeats its width across N columns; a `<colgroup width=W>` with no `<col>` children applies `W`
/// to the single column it represents (we don't model multi-column colgroup spans without `<col>`
/// — a documented simplification). Returns an empty vec when the table has no columns.
pub(crate) fn collect_col_widths(
    table: &LayoutBox,
    styles: &HashMap<dom::NodeId, style::ComputedStyle>,
) -> Vec<Option<f32>> {
    let mut widths: Vec<Option<f32>> = Vec::new();
    // `<col>`'s span comes from the same `TABLE_SPANS` snapshot used for cells' colspan (it stores
    // any element's colspan-like attribute), but `<col span>` uses the `span` attribute — read it
    // directly via `table_span(.., "colspan")` won't see `span`, so fall back to 1 here. We honor
    // an explicit width and a `span` value via the box's node attributes captured at build time.
    for child in &table.children {
        let d = style_of(child, styles).map(|cs| cs.display);
        match d {
            Some(style::Display::TableColumn) => {
                let w = style_of(child, styles).and_then(|cs| cs.width);
                let span = col_span_attr(child).max(1);
                for _ in 0..span {
                    widths.push(w);
                }
            }
            Some(style::Display::TableColumnGroup) => {
                // A colgroup with <col> children contributes those; otherwise itself = 1 column.
                let group_w = style_of(child, styles).and_then(|cs| cs.width);
                let mut had_col = false;
                for col in &child.children {
                    if style_of(col, styles).map(|cs| cs.display)
                        == Some(style::Display::TableColumn)
                    {
                        had_col = true;
                        let w = style_of(col, styles).and_then(|cs| cs.width).or(group_w);
                        let span = col_span_attr(col).max(1);
                        for _ in 0..span {
                            widths.push(w);
                        }
                    }
                }
                if !had_col {
                    widths.push(group_w);
                }
            }
            _ => {}
        }
    }
    widths
}

/// The `span` attribute of a `<col>`/`<colgroup>` box (default 1), read from the table-span
/// snapshot captured at build time (stored under the "colspan" channel).
pub(crate) fn col_span_attr(col: &LayoutBox) -> usize {
    let node = match col.node {
        Some(n) => n,
        None => return 1,
    };
    TABLE_SPANS.with(|t| t.borrow().get(&node).map(|(c, _)| (*c).max(1)).unwrap_or(1))
}

/// Append the `<tr>` rows found inside a row-group box (`thead`/`tbody`/`tfoot`) to `out`.
pub(crate) fn collect_group_rows(
    group: LayoutBox,
    styles: &HashMap<dom::NodeId, style::ComputedStyle>,
    out: &mut Vec<Vec<LayoutBox>>,
) {
    for row in group.children {
        if style_of(&row, styles).map(|cs| cs.display) == Some(style::Display::TableRow) {
            out.push(extract_cells(row, styles));
        }
    }
}

/// Extract the cell boxes (`display: table-cell`) that are children of one `<tr>` box.
pub(crate) fn extract_cells(
    row: LayoutBox,
    styles: &HashMap<dom::NodeId, style::ComputedStyle>,
) -> Vec<LayoutBox> {
    row.children
        .into_iter()
        .filter(|c| style_of(c, styles).map(|cs| cs.display) == Some(style::Display::TableCell))
        .collect()
}

/// The min-content width (px) of a cell: the widest single unbreakable word in its content
/// (so a column never gets narrower than its longest word), plus the cell's own horizontal
/// padding/border. Used as the lower bound for auto column sizing.
pub(crate) fn cell_min_content_width(boxx: &LayoutBox, measurer: &dyn TextMeasurer) -> f32 {
    let mut words: Vec<InlineWord> = Vec::new();
    collect_inline_words(&boxx.children, &mut words);
    let mut max_word = 0.0f32;
    for w in &words {
        for token in w.text.split_whitespace() {
            let ww = run_width(
                measurer,
                token,
                w.style.font_size,
                w.style.bold,
                w.style.letter_spacing,
                w.style.font_family.as_deref(),
            );
            max_word = max_word.max(ww);
        }
    }
    let p = boxx.dimensions.padding;
    let b = boxx.dimensions.border;
    max_word + p.left + p.right + b.left + b.right
}

/// Lay out a `display: table` box as a grid of cells. The box's content rect (x/y/width) is already
/// positioned by `layout_block`; this fills in cell geometry and returns the table's content height.
///
/// Algorithm (auto table layout, simplified but column-aligned):
///   1. Collect rows (descending thead→tbody→tfoot groups + direct `<tr>`s) and their cells,
///      honoring `colspan`/`rowspan` via an occupancy grid so spanned slots are skipped.
///   2. Column count = max over rows of sum(colspan). Column widths = max over the column's cells of
///      their preferred (max-content) width, floored by the min-content width; columns are then
///      grown to fill an explicit table width and shrunk (proportionally, but never below
///      min-content) to fit the available width.
///   3. Row heights = max laid-out cell height in the row (a rowspan cell contributes to the last
///      row it covers). Cells are laid out as block containers at (column x, row y).
///   4. A `<caption>` is laid out full-width above the rows.
pub(crate) fn layout_table(
    boxx: &mut LayoutBox,
    ctx: Ctx,
    styles: &HashMap<dom::NodeId, style::ComputedStyle>,
    measurer: &dyn TextMeasurer,
) -> f32 {
    // We need the document only for colspan/rowspan attributes; thread it via a thread-local-free
    // approach: those attrs were already validated into the style cascade? No — read from the DOM.
    // `layout_table` has no `doc`, so colspan/rowspan are read from a side channel set up by the
    // caller. Instead, we pull them from the cell box's node via the global doc passed through the
    // ctx-free path: we stored spans on build. To keep this self-contained, read them here from the
    // styles-independent attribute snapshot captured at build time (see `TABLE_SPANS`).
    let content = boxx.dimensions.content;
    let table_cs = style_of(boxx, styles).cloned().unwrap_or_default();

    // Border model. In the SEPARATE model, `border-spacing` opens a gap between adjacent cells (and
    // a margin between the cells and the table edge). In the COLLAPSE model cells are flush (no
    // spacing) and adjacent borders resolve to a single shared line (drawn by the painter). We thread
    // a single `spacing` scalar (0 when collapsed) through the column/row offset maths so the
    // geometry adapts; the painter reads `border_collapse` off each cell to draw single lines.
    let collapsed = table_cs.border_collapse == style::BorderCollapse::Collapse;
    let spacing = if collapsed {
        0.0
    } else {
        table_cs.border_spacing.max(0.0)
    };

    // --- 1. Pull out captions (laid out above the grid) and collect rows of cells. ---
    // Captions are direct table children with display: table-caption.
    let mut captions: Vec<LayoutBox> = Vec::new();
    {
        let mut kept: Vec<LayoutBox> = Vec::new();
        for child in std::mem::take(&mut boxx.children) {
            if style_of(&child, styles).map(|cs| cs.display) == Some(style::Display::TableCaption) {
                captions.push(child);
            } else {
                kept.push(child);
            }
        }
        boxx.children = kept;
    }
    // Explicit column widths from `<colgroup>`/`<col>` (read before the children are drained).
    let col_attr_widths = collect_col_widths(boxx, styles);
    let row_cells = collect_table_rows(boxx, styles);

    // --- 2. Assign cells to grid positions honoring colspan/rowspan via an occupancy grid. ---
    let mut cells: Vec<TableCell> = Vec::new();
    // occupied[(row, col)] -> covered by a spanning cell.
    let mut occupied: std::collections::HashSet<(usize, usize)> = std::collections::HashSet::new();
    let mut col_count = 0usize;
    for (r, cells_in_row) in row_cells.into_iter().enumerate() {
        let mut c = 0usize;
        for cell in cells_in_row {
            // Skip columns already covered by a rowspan from an earlier row.
            while occupied.contains(&(r, c)) {
                c += 1;
            }
            let colspan = table_span(&cell, "colspan");
            let rowspan = table_span(&cell, "rowspan");
            for dr in 0..rowspan {
                for dc in 0..colspan {
                    occupied.insert((r + dr, c + dc));
                }
            }
            col_count = col_count.max(c + colspan);
            cells.push(TableCell {
                boxx: cell,
                col: c,
                row: r,
                colspan,
                rowspan,
            });
            c += colspan;
        }
    }
    let num_rows = cells.iter().map(|c| c.row + c.rowspan).max().unwrap_or(0);
    if col_count == 0 || cells.is_empty() {
        // Empty table: lay out any captions and report their height.
        let h = layout_table_captions(&mut captions, content, ctx, styles, measurer);
        boxx.children = captions;
        return h;
    }

    // --- 3. Column widths (auto layout). ---
    // Per-column min-content and preferred (max-content) widths from single-column cells.
    let mut col_min = vec![0.0f32; col_count];
    let mut col_pref = vec![0.0f32; col_count];
    for cell in &cells {
        if cell.colspan == 1 {
            let c = cell.col;
            col_min[c] = col_min[c].max(cell_min_content_width(&cell.boxx, measurer));
            col_pref[c] = col_pref[c].max(intrinsic_width(&cell.boxx, styles, measurer));
        }
    }
    // Spanning cells: ensure the spanned columns together are wide enough for the cell's content.
    for cell in &cells {
        if cell.colspan > 1 {
            let start = cell.col;
            let end = (start + cell.colspan).min(col_count);
            let span_min: f32 = col_min[start..end].iter().sum();
            let span_pref: f32 = col_pref[start..end].iter().sum();
            let need_min = cell_min_content_width(&cell.boxx, measurer);
            let need_pref = intrinsic_width(&cell.boxx, styles, measurer);
            let n = (end - start) as f32;
            if need_min > span_min && n > 0.0 {
                let add = (need_min - span_min) / n;
                for w in &mut col_min[start..end] {
                    *w += add;
                }
            }
            if need_pref > span_pref && n > 0.0 {
                let add = (need_pref - span_pref) / n;
                for w in &mut col_pref[start..end] {
                    *w += add;
                }
            }
        }
    }
    // Apply explicit `<col>`/`<colgroup>` widths: a column with a declared width is pinned to it as
    // its preferred width (and at least its min-content, so content never overflows the column).
    for c in 0..col_count {
        if let Some(Some(w)) = col_attr_widths.get(c) {
            col_pref[c] = w.max(col_min[c]);
        }
    }

    // Ensure preferred >= min per column.
    for c in 0..col_count {
        col_pref[c] = col_pref[c].max(col_min[c]);
    }

    let sum_pref: f32 = col_pref.iter().sum();
    let sum_min: f32 = col_min.iter().sum();
    // Total inter-cell + edge spacing consumed horizontally (separated model). `spacing` is 0 when
    // collapsed, so this term vanishes and cells sit flush.
    let h_spacing_total = spacing * (col_count as f32 + 1.0);
    // Available width for the columns themselves (after reserving the spacing): an explicit table
    // width (clamped to the containing content width) else the table shrinks to its preferred width,
    // capped to the available content width.
    let avail = (content.width - h_spacing_total).max(0.0);
    let mut col_w = col_pref.clone();
    let target = match table_cs.width {
        Some(w) => (w - h_spacing_total).max(sum_min).min(avail.max(sum_min)),
        // A percentage width (e.g. `width: 100%`) is already resolved into `content.width`, so the
        // table fills the available width rather than shrinking to its preferred width.
        None if table_cs.width_pct.is_some() => avail.max(sum_min),
        None => sum_pref.min(avail),
    };
    if target > sum_pref && sum_pref > 0.0 {
        // Grow columns proportionally to fill the target width.
        let extra = target - sum_pref;
        for c in 0..col_count {
            let share = if sum_pref > 0.0 {
                col_pref[c] / sum_pref
            } else {
                1.0 / col_count as f32
            };
            col_w[c] = col_pref[c] + extra * share;
        }
    } else if target < sum_pref {
        // Shrink columns toward min-content, distributing the deficit by shrinkable slack.
        let shrinkable: f32 = (sum_pref - sum_min).max(0.0);
        let deficit = sum_pref - target.max(sum_min);
        if shrinkable > 0.0 && deficit > 0.0 {
            for c in 0..col_count {
                let slack = col_pref[c] - col_min[c];
                let take = if shrinkable > 0.0 {
                    deficit * (slack / shrinkable)
                } else {
                    0.0
                };
                col_w[c] = (col_pref[c] - take).max(col_min[c]);
            }
        } else {
            col_w = col_min.clone();
        }
    }

    // Column x offsets. In the separated model each column is preceded by `spacing` (and there's a
    // leading `spacing` before column 0); collapsed → `spacing == 0` so columns are flush. `col_x[c]`
    // is the left edge of column c's cell box; the table's used width includes the trailing spacing.
    let mut col_x = vec![0.0f32; col_count + 1];
    let mut x = spacing;
    for c in 0..col_count {
        col_x[c] = x;
        x += col_w[c] + spacing;
    }
    col_x[col_count] = x; // right edge incl. trailing spacing
    let cols_only: f32 = col_w.iter().sum();
    let table_width: f32 = cols_only + h_spacing_total;

    // --- Captions: `caption-side: bottom` ones go below the grid, the rest above. ---
    let (mut bottom_captions, mut top_captions): (Vec<LayoutBox>, Vec<LayoutBox>) = captions
        .drain(..)
        .partition(|c| {
            style_of(c, styles)
                .map(|cs| cs.caption_side_bottom)
                .unwrap_or(false)
        });
    let caption_h = layout_table_captions(&mut top_captions, content, ctx, styles, measurer);
    let grid_top = content.y + caption_h;

    // --- 4. Measure each cell's content height at its column width. ---
    // A cell's content box width = sum of its spanned columns minus the cell's own h-edges.
    let mut measured_h: Vec<f32> = vec![0.0; cells.len()];
    for (i, cell) in cells.iter_mut().enumerate() {
        let start = cell.col;
        let end = (start + cell.colspan).min(col_count);
        // A spanning cell also covers the inter-column spacing between the columns it spans.
        let last = end.saturating_sub(1).max(start);
        let cell_border_w: f32 = (col_x[last] + col_w[last] - col_x[start]).max(0.0);
        let m = cell.boxx.dimensions.margin;
        let b = cell.boxx.dimensions.border;
        let p = cell.boxx.dimensions.padding;
        let h_edges = m.left + m.right + b.left + b.right + p.left + p.right;
        let cw = (cell_border_w - h_edges).max(0.0);
        cell.boxx.dimensions.content.x = content.x + col_x[start] + m.left + b.left + p.left;
        cell.boxx.dimensions.content.y = grid_top + m.top + b.top + p.top;
        cell.boxx.dimensions.content.width = cw;
        let laid = layout_flex_item_contents(&mut cell.boxx, ctx, styles, measurer);
        // Honor an explicit cell height as a floor.
        let explicit_h = style_of(&cell.boxx, styles)
            .and_then(|cs| cs.height)
            .unwrap_or(0.0);
        measured_h[i] = laid.max(explicit_h);
    }

    // --- Row heights = max cell (border-box) height; rowspan distributes to the last covered row. ---
    let mut row_h = vec![0.0f32; num_rows];
    for (i, cell) in cells.iter().enumerate() {
        let b = cell.boxx.dimensions.border;
        let p = cell.boxx.dimensions.padding;
        let m = cell.boxx.dimensions.margin;
        let v_edges = m.top + m.bottom + b.top + b.bottom + p.top + p.bottom;
        let total = measured_h[i] + v_edges;
        if cell.rowspan <= 1 {
            let r = cell.row.min(num_rows.saturating_sub(1));
            row_h[r] = row_h[r].max(total);
        } else {
            // Distribute: ensure the rows it covers sum to at least its height.
            let start = cell.row;
            let end = (start + cell.rowspan).min(num_rows);
            let have: f32 = row_h[start..end].iter().sum();
            if total > have {
                let last = end.saturating_sub(1).max(start);
                if last < num_rows {
                    row_h[last] += total - have;
                }
            }
        }
    }

    // Grow the rows to fill a definite table height. `content.height` carries the table's definite
    // content height (set by `layout_block` for a `height`-constrained table; 0 when auto), so the
    // extra space above what the content needs is shared evenly across the rows — letting cells fill
    // a `height: 100px` table instead of shrinking to their content.
    let rows_total = row_h.iter().sum::<f32>() + spacing * (num_rows as f32 + 1.0);
    if num_rows > 0 && content.height > rows_total + 0.5 {
        let per = (content.height - rows_total) / num_rows as f32;
        for h in row_h.iter_mut() {
            *h += per;
        }
    }

    // Row y offsets. Like columns, each row is preceded by `spacing` in the separated model (with a
    // leading `spacing` above row 0); collapsed → flush. `row_y[r]` is row r's top.
    let mut row_y = vec![0.0f32; num_rows + 1];
    let mut y = spacing;
    for r in 0..num_rows {
        row_y[r] = y;
        y += row_h[r] + spacing;
    }
    row_y[num_rows] = y;
    let grid_h: f32 = y; // includes leading + trailing + inter-row spacing

    // --- Final placement: each cell fills its spanned column/row rect. ---
    for cell in &mut cells {
        let start_c = cell.col;
        let end_c = (start_c + cell.colspan).min(col_count);
        let start_r = cell.row.min(num_rows.saturating_sub(1));
        let end_r = (cell.row + cell.rowspan).min(num_rows);
        let last_c = end_c.saturating_sub(1).max(start_c);
        let last_r = end_r.saturating_sub(1).max(start_r);
        // Spanning cells also cover the inter-track spacing between the tracks they span.
        let cell_border_w: f32 = (col_x[last_c] + col_w[last_c] - col_x[start_c]).max(0.0);
        let cell_border_h: f32 = (row_y[last_r] + row_h[last_r] - row_y[start_r]).max(0.0);
        let m = cell.boxx.dimensions.margin;
        let b = cell.boxx.dimensions.border;
        let p = cell.boxx.dimensions.padding;
        let h_edges = m.left + m.right + b.left + b.right + p.left + p.right;
        let v_edges = m.top + m.bottom + b.top + b.bottom + p.top + p.bottom;
        let cw = (cell_border_w - h_edges).max(0.0);
        let ch = (cell_border_h - v_edges).max(0.0);
        let cx = content.x + col_x[start_c] + m.left + b.left + p.left;
        let cy = grid_top + row_y[start_r] + m.top + b.top + p.top;
        cell.boxx.dimensions.content = Rect {
            x: cx,
            y: cy,
            width: cw,
            height: ch,
        };
        // Re-lay the content into the (now taller) cell box. vertical-align defaults to top, so
        // content starts at the cell's content-box top (a documented simplification — middle/bottom
        // are not implemented).
        layout_flex_item_contents(&mut cell.boxx, ctx, styles, measurer);
    }

    // The table box is at least as wide as its columns. Record the used width.
    boxx.dimensions.content.width = table_width;

    // Collapsed-borders resolution is handled entirely in the painter (see `paint_box_opacity`):
    // each collapsed cell draws a 1px line on its left/top edges and on its OUTER right/bottom edge
    // coordinate, so a cell's right line and its neighbour's left line land on the SAME device pixel
    // (cells are flush) — a clean single-line grid instead of a doubled/gapped pair. (Documented
    // simplification: borders are not resolved per-edge by width; one line is drawn where any border
    // exists, using the cell's own border color.)

    // Rebuild the table box's children: captions first (above), then the flattened cells (the row /
    // row-group boxes were structural only — cells carry their own borders/backgrounds, so we drop
    // the wrappers and paint cells directly, mirroring how grid flattens its items).
    // `caption-side: bottom` captions sit below the grid.
    let bottom_origin = Rect {
        y: grid_top + grid_h,
        ..content
    };
    let bottom_caption_h =
        layout_table_captions(&mut bottom_captions, bottom_origin, ctx, styles, measurer);

    let mut new_children: Vec<LayoutBox> =
        Vec::with_capacity(top_captions.len() + cells.len() + bottom_captions.len());
    new_children.append(&mut top_captions);
    for cell in cells {
        new_children.push(cell.boxx);
    }
    new_children.append(&mut bottom_captions);
    boxx.children = new_children;

    caption_h + grid_h + bottom_caption_h
}

/// Lay out a table's `<caption>` boxes full-width above the grid, stacked. Returns their total
/// height. Each caption is positioned at the table's content origin and laid out as a block.
pub(crate) fn layout_table_captions(
    captions: &mut [LayoutBox],
    content: Rect,
    ctx: Ctx,
    styles: &HashMap<dom::NodeId, style::ComputedStyle>,
    measurer: &dyn TextMeasurer,
) -> f32 {
    let mut y = content.y;
    for cap in captions.iter_mut() {
        let containing = Rect {
            x: content.x,
            y,
            width: content.width,
            height: 0.0,
        };
        layout_block(cap, containing, ctx, styles, measurer);
        y += cap.dimensions.margin_box().height;
    }
    y - content.y
}
