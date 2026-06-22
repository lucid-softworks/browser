use crate::*;
use std::collections::HashMap;

// ---------------------------------------------------------------------------------------------
// Block layout
// ---------------------------------------------------------------------------------------------

/// Remove a leading list-item `Marker` child from `boxx` (if present), boxed. `#[inline(never)]`
/// so its locals don't enlarge the recursive `layout_block` stack frame.
#[inline(never)]
pub(crate) fn take_marker(boxx: &mut LayoutBox) -> Option<Box<LayoutBox>> {
    if matches!(
        boxx.children.first().map(|c| &c.content),
        Some(BoxContent::Marker(_))
    ) {
        Some(Box::new(boxx.children.remove(0)))
    } else {
        None
    }
}

/// Position a previously-extracted list-item marker `mb` to the LEFT of the box's content origin
/// (`x`, `y`) — in the list's left padding — and re-insert it as the first child for painting.
/// `#[inline(never)]` to keep `layout_block`'s frame small. The `mb` is taken boxed (despite
/// clippy's `boxed_local`) deliberately: it's the boxed `Option<Box<LayoutBox>>` held in the
/// deeply-recursive `layout_block` frame, so keeping it boxed avoids inflating that frame.
#[inline(never)]
#[allow(clippy::boxed_local)]
pub(crate) fn place_marker(
    boxx: &mut LayoutBox,
    mut mb: Box<LayoutBox>,
    x: f32,
    y: f32,
    measurer: &dyn TextMeasurer,
) {
    if let BoxContent::Marker(text) = &mb.content {
        let fs = if mb.style.font_size > 0.0 {
            mb.style.font_size
        } else {
            16.0
        };
        let fam = mb.style.font_family.as_deref();
        let mw = measurer.text_width(text, fs, mb.style.bold, fam);
        let gap = (fs * 0.5).max(4.0);
        let lh = mb
            .style
            .line_height
            .unwrap_or_else(|| measurer.line_height(fs, fam));
        mb.dimensions.content = Rect {
            x: (x - mw - gap).max(0.0),
            y,
            width: mw,
            height: lh,
        };
    }
    boxx.children.insert(0, *mb);
}

/// Lay out a block box (or anonymous/root block) given its containing block's content rect.
/// Fills `boxx.dimensions.content` (position + width + height) and recurses. Dispatches to the
/// flex / grid algorithm when this box establishes such a formatting context.
/// Resolve `auto` horizontal margins (CSS 2.2 §10.3.3) once the used width is known: with an explicit
/// width the free space goes to the auto margin(s), split evenly to center when both are auto (the
/// `margin: 0 auto` idiom). Persists the resolved margins (and `used_margins` for getComputedStyle).
/// `#[inline(never)]` so it doesn't bloat the recursive `layout_block` frame.
/// Distribute `free` horizontal space to the `auto` left/right margins (CSS 2.2 §10.3.3/§10.3.7):
/// both auto → split evenly (center); one auto → it takes all of `free`; neither → unchanged.
/// `margin_auto` is `[top, right, bottom, left]`.
pub(crate) fn distribute_auto_margins(margin: &mut Edges, margin_auto: [bool; 4], free: f32) {
    match (margin_auto[3], margin_auto[1]) {
        (true, true) => {
            let half = (free * 0.5).max(0.0);
            margin.left = half;
            margin.right = (free - half).max(0.0);
        }
        (true, false) => margin.left = (free - margin.right).max(0.0),
        (false, true) => margin.right = (free - margin.left).max(0.0),
        (false, false) => {}
    }
}

#[inline(never)]
pub(crate) fn resolve_block_margins(
    boxx: &mut LayoutBox,
    styles: &HashMap<dom::NodeId, style::ComputedStyle>,
    containing_width: f32,
    content_width: f32,
    has_explicit_width: bool,
) {
    let border = boxx.dimensions.border;
    let padding = boxx.dimensions.padding;
    let mut margin = boxx.dimensions.margin;
    let margin_auto = style_of(boxx, styles)
        .map(|cs| cs.margin_auto)
        .unwrap_or([false; 4]);
    if has_explicit_width {
        let free = containing_width
            - content_width
            - border.left
            - border.right
            - padding.left
            - padding.right;
        distribute_auto_margins(&mut margin, margin_auto, free);
        boxx.dimensions.margin = margin;
    }
    boxx.used_margins = Some([margin.top, margin.right, margin.bottom, margin.left]);
}

pub(crate) fn layout_block(
    boxx: &mut LayoutBox,
    containing: Rect,
    ctx: Ctx,
    styles: &HashMap<dom::NodeId, style::ComputedStyle>,
    measurer: &dyn TextMeasurer,
) {
    // A drawn form widget is replaced-like: its content size was fixed at build time. Place it like
    // an image (origin inside the containing block) and don't run block/inline formatting (which
    // would zero its pre-set height).
    if matches!(boxx.content, BoxContent::Widget(_) | BoxContent::Image(_)) {
        layout_image_box(boxx, containing);
        return;
    }

    let mut margin = boxx.dimensions.margin;
    let border = boxx.dimensions.border;
    let padding = boxx.dimensions.padding;

    // Explicit sizing comes from the box's DOM node's computed style (percentage width resolves
    // against the containing block's content width).
    let explicit_w = resolved_width(boxx, styles, containing.width);
    let explicit_h = explicit_height(boxx, styles);

    // Content width: containing content width minus this box's horizontal margin+border+padding,
    // unless an explicit width is set.
    let horizontal =
        margin.left + margin.right + border.left + border.right + padding.left + padding.right;
    let content_width = match explicit_w {
        Some(w) => w,
        None => (containing.width - horizontal).max(0.0),
    };
    // Clamp the used width to min-width / max-width (resolved against the containing block).
    let content_width = clamp_width(boxx, content_width, containing.width, styles);

    // Resolve `auto` horizontal margins now the used width is known (in a non-inlined helper so this
    // recursive frame stays small). Re-read the (possibly centered) margin for positioning.
    resolve_block_margins(
        boxx,
        styles,
        containing.width,
        content_width,
        explicit_w.is_some(),
    );
    margin = boxx.dimensions.margin;

    // Position: content origin sits inside the containing block, offset by left edges.
    let x = containing.x + margin.left + border.left + padding.left;
    let y = containing.y + margin.top + border.top + padding.top;

    boxx.dimensions.content = Rect {
        x,
        y,
        width: content_width,
        height: 0.0,
    };

    // List-item marker: pull a leading `Marker` child out of normal flow so it doesn't flow into the
    // content as a word. Positioned (in `place_marker`) once content height is known. Boxed +
    // `#[inline(never)]` helpers so the recursive `layout_block` frame stays small.
    let marker: Option<Box<LayoutBox>> = take_marker(boxx);

    // If this box is positioned (relative/absolute/fixed/sticky), it becomes the containing
    // block for its absolutely-positioned descendants. Update the context's `positioned` rect to
    // this box's padding box for children.
    let child_ctx = if !matches!(position_of(boxx, styles), style::Position::Static) {
        Ctx {
            positioned: boxx.dimensions.padding_box(),
            viewport: ctx.viewport,
        }
    } else {
        ctx
    };

    // Dispatch on the display mode of this box.
    let display = display_of(boxx, styles);
    let content_height = match display {
        style::Display::Flex | style::Display::InlineFlex => {
            layout_flex(boxx, child_ctx, styles, measurer)
        }
        style::Display::Grid | style::Display::InlineGrid => {
            layout_grid(boxx, child_ctx, styles, measurer)
        }
        style::Display::Table => layout_table(boxx, child_ctx, styles, measurer),
        _ => {
            // Block / inline-block / (anonymous, root): normal block-or-inline formatting.
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

    let final_height = explicit_h.unwrap_or(content_height);
    // Clamp the used height to min-height / max-height (% against the containing block height).
    let final_height = clamp_height(boxx, final_height, containing.height, styles);
    boxx.dimensions.content.height = final_height;

    // Position the list-item marker (if any) in the left padding, aligned with the first line.
    if let Some(mb) = marker {
        place_marker(boxx, mb, x, y, measurer);
    }

    // Resolve any out-of-flow children now that this box's geometry (and thus the containing
    // block for absolutes) is known. `child_ctx.positioned` was captured before this box's height
    // was resolved, so recompute the padding box here — otherwise `bottom`/`right` anchoring would
    // see a zero-height/zero-width containing block.
    let resolve_ctx = if !matches!(position_of(boxx, styles), style::Position::Static) {
        Ctx {
            positioned: boxx.dimensions.padding_box(),
            viewport: child_ctx.viewport,
        }
    } else {
        child_ctx
    };
    resolve_out_of_flow(boxx, resolve_ctx, styles, measurer);

    // Apply a `position: relative` offset (after normal flow, without affecting siblings).
    apply_relative_offset(boxx, containing, styles);
}

/// Lay out a block's block-level children top-to-bottom. Returns the total content height
/// (sum of child margin-box heights). No margin collapsing (kept simple). Out-of-flow children
/// are skipped here (they take no space) and resolved later.
pub(crate) fn layout_block_children(
    boxx: &mut LayoutBox,
    ctx: Ctx,
    styles: &HashMap<dom::NodeId, style::ComputedStyle>,
    measurer: &dyn TextMeasurer,
) -> f32 {
    let content = boxx.dimensions.content;
    let parent_align = text_align_of(boxx.node, styles);
    // `text-indent` applies to the first formatted line of the block container — i.e. the first
    // in-flow inline (anonymous) box. Tracked so later anonymous boxes aren't re-indented.
    let parent_indent = text_indent_of(boxx.node, styles);
    let mut indent_unused = parent_indent != 0.0;
    let mut cursor_y = content.y;
    // Floats placed by this container (its own block formatting context). Empty for the common
    // float-free page, in which case every helper below is a cheap no-op and the flow is unchanged.
    let mut floats = FloatCtx::new(content.x, content.x + content.width);
    for child in &mut boxx.children {
        if is_out_of_flow(child, styles) {
            continue; // resolved separately; takes no space in flow
        }

        let float = float_of(child, styles);
        let clear = clear_of(child, styles);
        // `clear` drops a box below the relevant earlier floats before it's placed.
        let start_y = if floats.is_empty() {
            cursor_y
        } else {
            floats.clear_to(clear, cursor_y)
        };

        if float != style::Float::None {
            layout_float_child(
                child,
                content,
                start_y,
                float,
                &mut floats,
                ctx,
                styles,
                measurer,
            );
            // A float doesn't advance normal flow, but a `clear` on it still moves the pen down so
            // following in-flow content starts below the cleared floats.
            cursor_y = cursor_y.max(start_y);
            continue;
        }

        // In-flow block: it stacks below previous siblings at the container's full content width.
        // (Per CSS a block box keeps its full width beside floats — only its *line boxes* shorten,
        // which inline layout handles separately; narrowing the box here would wrongly re-resolve a
        // percentage `width` against the reduced band.)
        cursor_y = start_y;
        // Each child's containing block is this box's content rect, but positioned so the
        // child stacks below previous siblings. We thread the running y via the containing
        // rect's y, and the child adds its own top margin/border/padding inside layout_block.
        let containing = Rect {
            x: content.x,
            y: cursor_y,
            width: content.width,
            height: 0.0,
        };
        match &child.content {
            BoxContent::Block => {
                grow_stack(|| layout_block(child, containing, ctx, styles, measurer))
            }
            BoxContent::Image(_) | BoxContent::Widget(_) => layout_image_box(child, containing),
            BoxContent::Anonymous => {
                // Anonymous blocks inherit the establishing block's text-align; the first one also
                // carries the block's `text-indent` (consumed so siblings aren't re-indented).
                let indent = if indent_unused {
                    indent_unused = false;
                    parent_indent
                } else {
                    0.0
                };
                layout_anonymous(
                    child,
                    containing,
                    parent_align,
                    indent,
                    ctx,
                    styles,
                    measurer,
                )
            }
            _ => {
                let indent = if indent_unused {
                    indent_unused = false;
                    parent_indent
                } else {
                    0.0
                };
                layout_anonymous(
                    child,
                    containing,
                    parent_align,
                    indent,
                    ctx,
                    styles,
                    measurer,
                );
            }
        }
        cursor_y += child.dimensions.margin_box().height;
    }
    // The container must be tall enough to contain its floats (it owns their formatting context).
    floats.max_bottom(cursor_y) - content.y
}

/// Lay out and place one floated child within `content` (the container's content rect), no higher
/// than `start_y`. Sizes the float (explicit/percentage `width`, else shrink-to-fit), lays out its
/// subtree, then positions its margin box via the [`FloatCtx`] (packing beside earlier floats and
/// wrapping down when a row is full).
#[allow(clippy::too_many_arguments)]
fn layout_float_child(
    child: &mut LayoutBox,
    content: Rect,
    start_y: f32,
    side: style::Float,
    floats: &mut FloatCtx,
    ctx: Ctx,
    styles: &HashMap<dom::NodeId, style::ComputedStyle>,
    measurer: &dyn TextMeasurer,
) {
    // Hand `layout_block` a containing rect whose width makes it resolve the float's used width
    // correctly, then reposition the result.
    //   * explicit / percentage `width`: pass the container's FULL content width so `layout_block`'s
    //     own `width` resolution matches (a percentage resolves against the containing block, not
    //     the reduced float band — passing a narrowed width would apply the percentage twice).
    //   * `auto` width: floats shrink-to-fit, which `layout_block` doesn't do, so pass a containing
    //     width equal to the shrink-to-fit margin-box width and let it fill that.
    let size_width = match resolved_width(child, styles, content.width) {
        Some(_) => content.width,
        None => {
            let b = child.dimensions.border;
            let p = child.dimensions.padding;
            let m = child.dimensions.margin;
            let own_edges = p.left + p.right + b.left + b.right;
            let h_edges = own_edges + m.left + m.right;
            let intrinsic = (intrinsic_width(child, styles, measurer) - own_edges).max(0.0);
            let avail = (content.width - h_edges).max(0.0);
            intrinsic.min(avail) + h_edges
        }
    };
    let size_rc = Rect {
        x: content.x,
        y: start_y,
        width: size_width,
        height: 0.0,
    };
    grow_stack(|| layout_block(child, size_rc, ctx, styles, measurer));
    let mb = child.dimensions.margin_box();
    // Place the margin box and slide the whole subtree from its tentative origin to the slot.
    let (fx, fy) = floats.place(mb.width, mb.height, start_y, side);
    shift_subtree(child, fx - mb.x, fy - mb.y);
}

/// Position a replaced (image) box within `containing`. The content size was pre-computed at
/// box-tree build time (from CSS width/height and/or the intrinsic size); here we only place the
/// content origin inside the containing block, offset by the box's own margin/border/padding.
pub(crate) fn layout_image_box(boxx: &mut LayoutBox, containing: Rect) {
    let m = boxx.dimensions.margin;
    let b = boxx.dimensions.border;
    let p = boxx.dimensions.padding;
    let x = containing.x + m.left + b.left + p.left;
    let y = containing.y + m.top + b.top + p.top;
    boxx.dimensions.content.x = x;
    boxx.dimensions.content.y = y;
    // width/height already set at build time; leave them.
}

/// The (right-edge x, top y) of the last line of inline content in `b`'s subtree, or `None` when
/// `b` holds no inline content. Used as the static-position origin for an absolutely-positioned box
/// that follows inline text among its siblings (CSS 2.2 §10.3.7): such a box's hypothetical in-flow
/// position is immediately after the preceding inline content, not at the container's top-left.
fn inline_end_position(b: &LayoutBox) -> Option<(f32, f32)> {
    fn visit(b: &LayoutBox, best: &mut Option<(f32, f32)>) {
        let is_inline_leaf = matches!(
            b.content,
            BoxContent::Text(_)
                | BoxContent::Image(_)
                | BoxContent::Widget(_)
                | BoxContent::Caret
                | BoxContent::Marker(_)
        );
        if is_inline_leaf {
            let r = b.dimensions.border_box();
            let cand = (r.x + r.width, r.y);
            // Prefer the lowest line (largest y); within the same line, the furthest-right edge.
            let take = match *best {
                Some((bx, by)) => {
                    cand.1 > by + 0.01 || ((cand.1 - by).abs() <= 0.01 && cand.0 > bx)
                }
                None => true,
            };
            if take {
                *best = Some(cand);
            }
        }
        for c in &b.children {
            visit(c, best);
        }
    }
    let mut best = None;
    visit(b, &mut best);
    best
}

/// Resolve out-of-flow (absolute / fixed) children of `boxx`: size and position them against
/// their containing block, then lay out their own children.
pub(crate) fn resolve_out_of_flow(
    boxx: &mut LayoutBox,
    ctx: Ctx,
    styles: &HashMap<dom::NodeId, style::ComputedStyle>,
    measurer: &dyn TextMeasurer,
) {
    // The out-of-flow box's static position (used for the resolved value when both opposite insets
    // are `auto`) is its hypothetical in-flow origin — approximated by this parent's content-box
    // top-left. Captured before the loop so each child sees the same parent content rect.
    let parent_content = boxx.dimensions.content;
    // Tracks where in-flow inline content among the siblings ended, so an abspos that follows it
    // gets the static x/y immediately after that content (e.g. `12345<span style=position:absolute>`
    // sits after "12345", not at the container origin). Reset by a real block, which starts a line.
    let mut inline_cursor: Option<(f32, f32)> = None;
    for i in 0..boxx.children.len() {
        match position_of(&boxx.children[i], styles) {
            style::Position::Absolute => {
                let child = &mut boxx.children[i];
                layout_out_of_flow(
                    child,
                    ctx.positioned,
                    parent_content,
                    inline_cursor,
                    ctx,
                    styles,
                    measurer,
                );
            }
            style::Position::Fixed => {
                let child = &mut boxx.children[i];
                layout_out_of_flow(
                    child,
                    ctx.viewport,
                    parent_content,
                    inline_cursor,
                    ctx,
                    styles,
                    measurer,
                );
            }
            // In-flow sibling (static / relative / sticky): advance the inline cursor past its
            // inline content; a real block box resets it (the next abspos would start a new line).
            _ => match &boxx.children[i].content {
                BoxContent::Block => inline_cursor = None,
                _ => {
                    if let Some(end) = inline_end_position(&boxx.children[i]) {
                        inline_cursor = Some(end);
                    }
                }
            },
        }
    }
}

/// Lay out an out-of-flow box against `cb` (its containing block rect = padding box of the
/// nearest positioned ancestor, or the viewport). Insets resolve the position; size comes from
/// explicit width/height or content.
#[allow(clippy::too_many_arguments)]
pub(crate) fn layout_out_of_flow(
    boxx: &mut LayoutBox,
    cb: Rect,
    parent_content: Rect,
    // Static-position origin from preceding inline siblings `(x, y)`, when this box follows inline
    // content. Overrides `cb`/`parent_content` for the axis (or axes) whose insets are both `auto`.
    inline_static: Option<(f32, f32)>,
    ctx: Ctx,
    styles: &HashMap<dom::NodeId, style::ComputedStyle>,
    measurer: &dyn TextMeasurer,
) {
    let cs = match style_of(boxx, styles) {
        Some(cs) => cs.clone(),
        None => return,
    };
    let mut margin = boxx.dimensions.margin;
    let border = boxx.dimensions.border;
    let padding = boxx.dimensions.padding;
    let horizontal =
        margin.left + margin.right + border.left + border.right + padding.left + padding.right;

    // Replaced content (<img>/<canvas>/<svg>/form widget) has an intrinsic size already resolved
    // at build time (CSS width/height + intrinsic dims + aspect ratio, via `image_content_size`).
    // Out-of-flow layout must preserve those dimensions: treating the box as a container would
    // size width from `intrinsic_width` (0 for a childless box) and height from children (also 0),
    // leaving the element invisible — this is why absolutely-positioned images didn't render.
    let replaced = matches!(boxx.content, BoxContent::Image(_) | BoxContent::Widget(_));
    let (replaced_w, replaced_h) = (
        boxx.dimensions.content.width,
        boxx.dimensions.content.height,
    );

    // Resolve insets against the containing block. Percentage (and percentage-bearing `calc()`)
    // insets can't be resolved at cascade time because their basis — the containing block's
    // extent on the relevant axis — isn't known until now, so they're carried symbolically in
    // `*_spec` and resolved here: left/right against `cb.width`, top/bottom against `cb.height`.
    // Fall back to the pre-resolved px field for any path that set only it (e.g. the `inset`
    // shorthand, which stores absolute lengths directly without a spec).
    let inset_left = cs.left_spec.resolve_px(cb.width).or(cs.left);
    let inset_right = cs.right_spec.resolve_px(cb.width).or(cs.right);
    let inset_top = cs.top_spec.resolve_px(cb.height).or(cs.top);
    let inset_bottom = cs.bottom_spec.resolve_px(cb.height).or(cs.bottom);

    // Content width:
    //   * explicit `width` wins;
    //   * `left` AND `right` both set with no width => stretch to fill between them;
    //   * otherwise shrink-to-fit to the box's intrinsic (max-content) width, exactly like the
    //     inline-block path. `intrinsic_width` returns the border-box width (content + padding +
    //     border), so we strip the box's own horizontal padding/border back off to get content.
    let content_width = if replaced {
        replaced_w
    } else if let Some(w) = cs.width {
        w
    } else if let (Some(l), Some(r)) = (inset_left, inset_right) {
        (cb.width - l - r - horizontal).max(0.0)
    } else {
        let intrinsic = intrinsic_width(boxx, styles, measurer);
        let own_edges = padding.left + padding.right + border.left + border.right;
        // Never wider than the containing block's available content width.
        let avail = (cb.width - horizontal).max(0.0);
        (intrinsic - own_edges).max(0.0).min(avail)
    };
    // Clamp to min/max-width against the containing block.
    let content_width = clamp_width(boxx, content_width, cb.width, styles);

    // Over-constrained box (left, right AND width all set) with auto margin(s): the leftover inline
    // space goes to the auto margin(s) (CSS 2.2 §10.3.7) — centering when both are auto. Otherwise
    // auto margins stay 0 (the style crate already resolved them so).
    if let (Some(l), Some(r)) = (inset_left, inset_right) {
        if cs.width.is_some() && (cs.margin_auto[1] || cs.margin_auto[3]) {
            let free = cb.width
                - l
                - r
                - content_width
                - border.left
                - border.right
                - padding.left
                - padding.right;
            distribute_auto_margins(&mut margin, cs.margin_auto, free);
            boxx.dimensions.margin = margin;
        }
    }
    boxx.used_margins = Some([margin.top, margin.right, margin.bottom, margin.left]);

    // Tentative content origin: relative to the containing block's top-left, offset by insets.
    // The insets address the box's *margin* box edge; we then add the box's own left/top edges.
    let border_left_x = if let Some(l) = inset_left {
        cb.x + l + margin.left
    } else if let Some(r) = inset_right {
        cb.x + cb.width - r - (content_width + horizontal) + margin.left
    } else {
        // Both horizontal insets auto → static position: after preceding inline content if any.
        inline_static.map(|(x, _)| x).unwrap_or(cb.x)
    };
    let border_top_y = if let Some(t) = inset_top {
        cb.y + t
    } else if let Some(b) = inset_bottom {
        cb.y + cb.height - b // adjusted after height is known below
    } else {
        inline_static.map(|(_, y)| y).unwrap_or(cb.y)
    };

    let x = border_left_x + margin.left + border.left + padding.left;
    let y = border_top_y + margin.top + border.top + padding.top;
    boxx.dimensions.content = Rect {
        x,
        y,
        width: content_width,
        height: 0.0,
    };

    // This box is itself positioned, so it's the containing block for its abs descendants.
    let child_ctx = Ctx {
        positioned: boxx.dimensions.padding_box(),
        viewport: ctx.viewport,
    };

    let display = display_of(boxx, styles);
    let content_height = if replaced {
        // Replaced box: its height is the build-time intrinsic height, not derived from children.
        replaced_h
    } else {
        match display {
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
        }
    };
    // For replaced content the build-time height already honors CSS `height`; otherwise CSS
    // `height` overrides the content-derived height.
    let final_height = if replaced {
        content_height
    } else {
        cs.height.unwrap_or(content_height)
    };
    let final_height = clamp_height(boxx, final_height, cb.height, styles);
    boxx.dimensions.content.height = final_height;

    // If positioned by `bottom` (no `top`), re-anchor now that height is known.
    if inset_top.is_none() {
        if let Some(b) = inset_bottom {
            let new_border_top = cb.y + cb.height
                - b
                - (final_height
                    + margin.top
                    + margin.bottom
                    + border.top
                    + border.bottom
                    + padding.top
                    + padding.bottom);
            let new_y = new_border_top + margin.top + border.top + padding.top;
            shift_subtree(boxx, 0.0, new_y - boxx.dimensions.content.y);
        }
    }
    // Similarly for `right` with no `left` already handled above for x.

    // Record the CSSOM *used* inset values. For each axis, a set side resolves to the offset from
    // the containing block edge to the box's margin-box edge; when BOTH opposite sides are `auto`
    // the box sits at its static position, so the used value is measured from the static origin
    // (the parent's content-box top-left) instead of the laid-out (cb-origin) position.
    let mb = boxx.dimensions.margin_box();
    let (vert_auto, horiz_auto) = (
        (inset_top.is_none() && inset_bottom.is_none()),
        (inset_left.is_none() && inset_right.is_none()),
    );
    // Vertical: margin-box top relative to cb top (or static origin when both auto — preceding
    // inline content if any, else the parent content-box edge).
    let mb_top = if vert_auto {
        inline_static.map(|(_, y)| y).unwrap_or(parent_content.y)
    } else {
        mb.y
    };
    let mb_left = if horiz_auto {
        inline_static.map(|(x, _)| x).unwrap_or(parent_content.x)
    } else {
        mb.x
    };
    let used_top = mb_top - cb.y;
    let used_left = mb_left - cb.x;
    let used_bottom = (cb.y + cb.height) - (mb_top + mb.height);
    let used_right = (cb.x + cb.width) - (mb_left + mb.width);
    boxx.used_insets = Some([used_top, used_right, used_bottom, used_left]);

    // Recompute the containing block for nested absolutes now that this box's height (and any
    // `bottom` re-anchor shift) is final.
    let resolve_ctx = Ctx {
        positioned: boxx.dimensions.padding_box(),
        viewport: ctx.viewport,
    };
    resolve_out_of_flow(boxx, resolve_ctx, styles, measurer);
}

/// Apply a `position: relative` offset to `boxx` and its whole subtree by the resolved
/// (left/right, top/bottom) insets, without affecting siblings.
pub(crate) fn apply_relative_offset(
    boxx: &mut LayoutBox,
    containing: Rect,
    styles: &HashMap<dom::NodeId, style::ComputedStyle>,
) {
    let cs = match style_of(boxx, styles) {
        Some(cs) => cs,
        None => return,
    };
    if cs.position != style::Position::Relative {
        return;
    }
    // Percentage insets resolve against the containing block (width for left/right, height for
    // top/bottom); `*_spec` carries them symbolically until now. Fall back to the pre-resolved px
    // field for any path that set only it (e.g. the `inset` shorthand).
    let dx = if let Some(l) = cs.left_spec.resolve_px(containing.width).or(cs.left) {
        l
    } else if let Some(r) = cs.right_spec.resolve_px(containing.width).or(cs.right) {
        -r
    } else {
        0.0
    };
    let dy = if let Some(t) = cs.top_spec.resolve_px(containing.height).or(cs.top) {
        t
    } else if let Some(b) = cs.bottom_spec.resolve_px(containing.height).or(cs.bottom) {
        -b
    } else {
        0.0
    };
    if dx != 0.0 || dy != 0.0 {
        shift_subtree(boxx, dx, dy);
    }
}

/// Translate a box and all its descendants by (dx, dy).
pub(crate) fn shift_subtree(boxx: &mut LayoutBox, dx: f32, dy: f32) {
    boxx.dimensions.content.x += dx;
    boxx.dimensions.content.y += dy;
    for c in &mut boxx.children {
        shift_subtree(c, dx, dy);
    }
}

/// An anonymous block: same geometry rules as a block but with zero margins/border/padding and
/// inline-only children.
pub(crate) fn layout_anonymous(
    boxx: &mut LayoutBox,
    containing: Rect,
    align: TextAlignLocal,
    text_indent: f32,
    ctx: Ctx,
    styles: &HashMap<dom::NodeId, style::ComputedStyle>,
    measurer: &dyn TextMeasurer,
) {
    boxx.dimensions.margin = Edges::default();
    boxx.dimensions.border = Edges::default();
    boxx.dimensions.padding = Edges::default();
    boxx.dimensions.content = Rect {
        x: containing.x,
        y: containing.y,
        width: containing.width,
        height: 0.0,
    };
    let h = layout_inline_children(boxx, align, text_indent, ctx, styles, measurer);
    boxx.dimensions.content.height = h;
}
