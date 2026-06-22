use crate::*;

/// Recursively paint a layout box and its children, translating every box by the fixed
/// pixel offset `(ox, oy)` and vertically clipping to `[clip_top, clip_bottom]`.
///
/// For each box, in order: (a) fill the border box with `background_color` (if any);
/// (b) paint the four border edges; (c) draw text content at the content rect. Then recurse.
#[allow(clippy::too_many_arguments)]
pub(crate) fn paint_box(
    fb: &mut Framebuffer,
    font: &dyn GlyphRasterizer,
    b: &layout::LayoutBox,
    ox: f32,
    oy: f32,
    clip_top: f32,
    clip_bottom: f32,
    images: &HashMap<dom::NodeId, DecodedImage>,
    canvas_bitmaps: &HashMap<dom::NodeId, DecodedImage>,
    svg_bitmaps: &HashMap<dom::NodeId, DecodedImage>,
    mask_bitmaps: &HashMap<dom::NodeId, DecodedImage>,
    sel_ranges: &[Option<(usize, usize)>],
    run_idx: &mut usize,
) {
    // The base device-space transform is a pure translation by the scroll offset. CSS `transform`
    // declarations compose additional affines on top per-box.
    let xf = Affine::translate(ox, oy);
    paint_box_opacity(
        fb,
        font,
        b,
        &xf,
        clip_top,
        clip_bottom,
        images,
        canvas_bitmaps,
        svg_bitmaps,
        mask_bitmaps,
        1.0,
        sel_ranges,
        run_idx,
    );
}

/// A 2D affine mapping a CSS-space point `(x, y)` to a device-space point: `x' = a*x + c*y + e`,
/// `y' = b*x + d*y + f`. Used to remap painted geometry for CSS `transform` (and to carry the
/// scroll translation). Translate + scale stay axis-aligned (painted exactly); rotation/skew make
/// it non-axis-aligned (background/border rects rasterized as transformed quads; text is positioned
/// by the transform but glyphs are not themselves rotated — see [`paint_box_opacity`]).
#[derive(Clone, Copy, Debug)]
pub(crate) struct Affine {
    a: f32,
    b: f32,
    c: f32,
    d: f32,
    e: f32,
    f: f32,
}

impl Affine {
    fn translate(tx: f32, ty: f32) -> Affine {
        Affine {
            a: 1.0,
            b: 0.0,
            c: 0.0,
            d: 1.0,
            e: tx,
            f: ty,
        }
    }
    /// Map a CSS point to device space.
    pub(crate) fn apply(&self, x: f32, y: f32) -> (f32, f32) {
        (
            self.a * x + self.c * y + self.e,
            self.b * x + self.d * y + self.f,
        )
    }
    /// `self` ∘ `m`: apply `m` first (in CSS space), then `self`.
    fn then(&self, m: &Affine) -> Affine {
        Affine {
            a: self.a * m.a + self.c * m.b,
            b: self.b * m.a + self.d * m.b,
            c: self.a * m.c + self.c * m.d,
            d: self.b * m.c + self.d * m.d,
            e: self.a * m.e + self.c * m.f + self.e,
            f: self.b * m.e + self.d * m.f + self.f,
        }
    }
    /// True if the linear part has no rotation/skew (axis-aligned: b == c == 0), so a rect stays a
    /// rect and can be filled with the fast axis-aligned primitives.
    fn is_axis_aligned(&self) -> bool {
        self.b.abs() < 1e-4 && self.c.abs() < 1e-4
    }
}

/// Map a CSS-space rect `(x, y, w, h)` through an axis-aligned affine into a device `Rect`.
/// Caller must ensure `xf.is_axis_aligned()`.
pub(crate) fn xf_rect(xf: &Affine, x: f32, y: f32, w: f32, h: f32) -> Rect {
    let (x0, y0) = xf.apply(x, y);
    let (x1, y1) = xf.apply(x + w, y + h);
    let (lx, rx) = (x0.min(x1), x0.max(x1));
    let (ty, by) = (y0.min(y1), y0.max(y1));
    Rect {
        x: lx.round() as i32,
        y: ty.round() as i32,
        w: (rx - lx).round() as i32,
        h: (by - ty).round() as i32,
    }
}

/// Scale a u8 alpha by an effective opacity in 0.0..=1.0.
pub(crate) fn scale_alpha(a: u8, opacity: f32) -> u8 {
    ((a as f32) * opacity.clamp(0.0, 1.0))
        .round()
        .clamp(0.0, 255.0) as u8
}

/// Paint a box, multiplying every painted alpha by `effective_opacity` (the product of this
/// box's and all ancestor `opacity` values). This approximates group opacity without an offscreen
/// layer: each fill/blit/glyph is composited at the scaled alpha rather than the whole subtree
/// being flattened first, so overlapping descendants may show seams — acceptable for our purposes.
#[allow(clippy::too_many_arguments)]
pub(crate) fn paint_box_opacity(
    fb: &mut Framebuffer,
    font: &dyn GlyphRasterizer,
    b: &layout::LayoutBox,
    xf: &Affine,
    clip_top: f32,
    clip_bottom: f32,
    images: &HashMap<dom::NodeId, DecodedImage>,
    canvas_bitmaps: &HashMap<dom::NodeId, DecodedImage>,
    svg_bitmaps: &HashMap<dom::NodeId, DecodedImage>,
    mask_bitmaps: &HashMap<dom::NodeId, DecodedImage>,
    parent_opacity: f32,
    sel_ranges: &[Option<(usize, usize)>],
    run_idx: &mut usize,
) {
    // This box's opacity multiplies into the inherited (effective) opacity for itself + subtree.
    let opacity = parent_opacity * b.style.opacity.clamp(0.0, 1.0);

    let border = b.dimensions.border_box();
    let content = b.dimensions.content;
    let radius = b.style.border_radius();
    let extras = b.style.extras.as_deref();

    // Fast-path: the common no-transform box keeps the incoming affine. A CSS `transform` composes
    // an extra affine pivoted at the transform-origin (in this box's border-box space), so the box
    // *and its whole subtree* are remapped by that affine (translate/scale exactly; rotate/skew via
    // transformed quads for fills).
    let local_xf;
    let xf: &Affine = if let Some(m) = extras.and_then(|e| e.transform) {
        let origin = extras.map(|e| e.transform_origin).unwrap_or((0.5, 0.5));
        let ox = border.x + origin.0 * border.width;
        let oy = border.y + origin.1 * border.height;
        // Resolve percentage translates against the box size (parser left them at 0; CSS uses the
        // element's own size). We re-resolve here by scaling the affine's translate columns — but
        // since the parser already produced absolute px for px translates, only the matrix's e/f
        // (which were 0 for %), this is a no-op for the common case. Keeping it simple: apply m
        // about (ox, oy): T(ox,oy) * m * T(-ox,-oy).
        let pivot = Affine {
            a: m[0],
            b: m[1],
            c: m[2],
            d: m[3],
            e: m[4],
            f: m[5],
        };
        let to_origin = Affine::translate(ox, oy);
        let from_origin = Affine::translate(-ox, -oy);
        local_xf = xf.then(&to_origin.then(&pivot).then(&from_origin));
        &local_xf
    } else {
        xf
    };

    let axis = xf.is_axis_aligned();
    // Device-space vertical extent of the box (for the visible-band clip). With a non-axis-aligned
    // transform we conservatively take the bounding box of the four mapped corners.
    let (top, bottom) = {
        let y0 = border.y.min(content.y);
        let y1 = (border.y + border.height).max(content.y + content.height);
        let x0 = border.x.min(content.x);
        let x1 = (border.x + border.width).max(content.x + content.width);
        let corners = [
            xf.apply(x0, y0),
            xf.apply(x1, y0),
            xf.apply(x0, y1),
            xf.apply(x1, y1),
        ];
        let mut t = f32::MAX;
        let mut bt = f32::MIN;
        for (_, cy) in corners {
            t = t.min(cy);
            bt = bt.max(cy);
        }
        (t, bt)
    };
    let offscreen = bottom < clip_top || top >= clip_bottom;

    // Cull the whole offscreen subtree — don't even recurse — so a long page (e.g. browserscore's
    // 14M-px DOM with every <details> open) doesn't walk thousands of off-viewport boxes per frame.
    // In-flow children are contained in their parent's (affine-mapped) vertical extent, so an
    // offscreen box's descendants are offscreen too. Skipped only when there's no active text
    // selection: the selection highlight indexes text runs by global DFS position, and skipping a
    // subtree would desync that counter (when nothing is selected, `run_idx` is unused).
    if offscreen && sel_ranges.is_empty() {
        return;
    }

    // `visibility: hidden`/`collapse` keeps the box in layout (so children still position and the
    // run counter still advances below) but paints none of its OWN content. Children are recursed
    // regardless — `visibility` inherits, but a descendant can opt back in with `visibility: visible`.
    if !offscreen && opacity > 0.0 && b.style.visible {
        // (0) OUTER box-shadows: painted BEFORE the background so the box sits on top.
        if let Some(ex) = extras {
            for sh in &ex.box_shadows {
                if !sh.inset {
                    paint_box_shadow(fb, xf, border, radius, sh, opacity, false);
                }
            }
        }

        // (a) Background fills the border box: a gradient if set, else the solid color. When the box
        // carries a `mask-image`, the background is composited only through the mask's opaque pixels
        // (the icon technique: `background: currentColor; mask: url(icon.svg)`) — see `paint_masked_bg`.
        let mask_cov = b.node.and_then(|n| mask_bitmaps.get(&n));
        if let Some(cov) = mask_cov {
            if let Some((r, g, bl)) = b.style.background_color {
                // Masked solid background: stamp the color through the coverage alpha.
                let c = Color {
                    r,
                    g,
                    b: bl,
                    a: scale_alpha(255, opacity),
                };
                paint_masked_bg(fb, xf, border, cov, c, axis);
            } else if let Some(grad) = extras.and_then(|e| e.background_gradient.as_ref()) {
                // Gradient-as-background under a mask is out of scope (the icon technique uses a solid
                // color). Paint the gradient unmasked so something shows. Rare.
                paint_gradient_fill(fb, xf, border, radius, grad, opacity, axis);
            }
        } else if let Some(grad) = extras.and_then(|e| e.background_gradient.as_ref()) {
            paint_gradient_fill(fb, xf, border, radius, grad, opacity, axis);
        } else if let Some((r, g, bl)) = b.style.background_color {
            let c = Color {
                r,
                g,
                b: bl,
                a: scale_alpha(255, opacity),
            };
            fill_box(
                fb,
                xf,
                border.x,
                border.y,
                border.width,
                border.height,
                radius,
                c,
                axis,
            );
        }

        // (b) Borders. For a collapsed table CELL, draw single shared 1px lines: left/top at the
        // border-box edges and right/bottom at the OUTER edge coordinate, so a cell's right line and
        // its flush neighbour's left line land on the same device pixel (a clean single-line grid,
        // not a doubled/gapped pair). Otherwise: the normal four filled edge rects.
        let e = b.dimensions.border;
        let ba = scale_alpha(255, opacity);
        let bc = Color {
            r: b.style.border_color.0,
            g: b.style.border_color.1,
            b: b.style.border_color.2,
            a: ba,
        };
        let collapsed_cell = b.style.is_table_cell
            && b.style.border_collapse == style::BorderCollapse::Collapse
            && (e.top > 0.0 || e.right > 0.0 || e.bottom > 0.0 || e.left > 0.0);
        if collapsed_cell {
            let lw = 1.0f32; // single hairline regardless of declared width (simplified resolution)
                             // left + top at the border-box edges
            fill_box(
                fb,
                xf,
                border.x,
                border.y,
                lw,
                border.height + lw,
                0.0,
                bc,
                axis,
            );
            fill_box(
                fb,
                xf,
                border.x,
                border.y,
                border.width + lw,
                lw,
                0.0,
                bc,
                axis,
            );
            // right + bottom at the OUTER edge coordinate (coincides with the neighbour's left/top)
            fill_box(
                fb,
                xf,
                border.x + border.width,
                border.y,
                lw,
                border.height + lw,
                0.0,
                bc,
                axis,
            );
            fill_box(
                fb,
                xf,
                border.x,
                border.y + border.height,
                border.width + lw,
                lw,
                0.0,
                bc,
                axis,
            );
        } else {
            if e.top > 0.0 {
                fill_box(
                    fb,
                    xf,
                    border.x,
                    border.y,
                    border.width,
                    e.top,
                    radius.min(e.top.max(1.0)),
                    bc,
                    axis,
                );
            }
            if e.bottom > 0.0 {
                fill_box(
                    fb,
                    xf,
                    border.x,
                    border.y + border.height - e.bottom,
                    border.width,
                    e.bottom,
                    radius.min(e.bottom.max(1.0)),
                    bc,
                    axis,
                );
            }
            if e.left > 0.0 {
                fill_box(
                    fb,
                    xf,
                    border.x,
                    border.y,
                    e.left,
                    border.height,
                    0.0,
                    bc,
                    axis,
                );
            }
            if e.right > 0.0 {
                fill_box(
                    fb,
                    xf,
                    border.x + border.width - e.right,
                    border.y,
                    e.right,
                    border.height,
                    0.0,
                    bc,
                    axis,
                );
            }
        }

        // (a2) INSET box-shadows: painted inside the box AFTER the background (best-effort: a
        // feathered inner band, no rounded clipping).
        if let Some(ex) = extras {
            for sh in &ex.box_shadows {
                if sh.inset {
                    paint_box_shadow(fb, xf, border, radius, sh, opacity, true);
                }
            }
        }

        // (c) Text content, at the content rect's baseline. Don't paint into the console area.
        // Text is positioned through the affine's mapped origin; glyphs are not rotated (NOTE:
        // rotated/skewed text is positioned but rendered upright — an approximation).
        if let layout::BoxContent::Text(s) = &b.content {
            let (dx, dy) = xf.apply(content.x, content.y);
            if dy < clip_bottom {
                // Scale font size by the affine's average linear magnitude so scale() enlarges text.
                let sx = (xf.a * xf.a + xf.b * xf.b).sqrt();
                let sy = (xf.c * xf.c + xf.d * xf.d).sqrt();
                let scale = ((sx + sy) * 0.5).max(0.01);
                let fs = b.style.font_size * scale;
                let ta = scale_alpha(255, opacity);
                let color = Color {
                    r: b.style.color.0,
                    g: b.style.color.1,
                    b: b.style.color.2,
                    a: ta,
                };
                let x = dx;
                let baseline = dy + fs * 0.8;
                // Selection highlight: if this run (identified by its DFS index) has a selected
                // character sub-range, fill a translucent rect behind those glyphs BEFORE drawing
                // the text so the glyphs stay legible on top. Advance widths use the SAME scaled
                // font size + letter-spacing the glyph painter uses, so the band lines up exactly.
                if !s.is_empty() {
                    if let Some(Some((cs, ce))) = sel_ranges.get(*run_idx) {
                        let ls = b.style.letter_spacing * scale;
                        let mut hx0 = x;
                        let mut pen = x;
                        for (i, ch) in s.chars().enumerate() {
                            if i == *cs {
                                hx0 = pen;
                            }
                            pen += font.advance(ch, fs) + ls;
                            if i + 1 == *ce {
                                break;
                            }
                        }
                        // If the range starts at 0, hx0 stays at the run's left edge.
                        let hx1 = pen;
                        let top = dy.round() as i32;
                        let h = (fs * 1.25).round().max(1.0) as i32;
                        let w = (hx1 - hx0).round() as i32;
                        if w > 0 {
                            // A translucent macOS-ish selection blue, composited over the text bg.
                            let hl = Color {
                                r: 74,
                                g: 144,
                                b: 255,
                                a: scale_alpha(102, opacity),
                            };
                            fb.fill_rect(
                                Rect {
                                    x: hx0.round() as i32,
                                    y: top,
                                    w,
                                    h,
                                },
                                hl,
                            );
                        }
                    }
                }
                let end_x = draw_run(
                    fb,
                    font,
                    s,
                    x,
                    baseline,
                    fs,
                    color,
                    b.style.bold,
                    b.style.letter_spacing * scale,
                );
                let run_w = (end_x - x).max(0.0);
                if run_w > 0.0 {
                    let thickness = (fs / 14.0).clamp(1.0, 2.0).round().max(1.0) as i32;
                    if b.style.underline {
                        let uy = (baseline + 1.0).round() as i32;
                        fb.fill_rect(
                            Rect {
                                x: x.round() as i32,
                                y: uy,
                                w: run_w.round() as i32,
                                h: thickness,
                            },
                            color,
                        );
                    }
                    if b.style.line_through {
                        let my = (baseline - fs * 0.3).round() as i32;
                        fb.fill_rect(
                            Rect {
                                x: x.round() as i32,
                                y: my,
                                w: run_w.round() as i32,
                                h: thickness,
                            },
                            color,
                        );
                    }
                    if b.style.overline {
                        // A line above the text, near the top of the em box (~0.8em above baseline).
                        let oy = (baseline - fs * 0.8).round() as i32;
                        fb.fill_rect(
                            Rect {
                                x: x.round() as i32,
                                y: oy,
                                w: run_w.round() as i32,
                                h: thickness,
                            },
                            color,
                        );
                    }
                }
            }
        }

        // (c1b) List-item marker: a bullet/number drawn like a text run at the marker's content
        // origin (positioned by layout in the list's left padding). No selection handling.
        if let layout::BoxContent::Marker(s) = &b.content {
            let (dx, dy) = xf.apply(content.x, content.y);
            if dy < clip_bottom && !s.is_empty() {
                let sx = (xf.a * xf.a + xf.b * xf.b).sqrt();
                let sy = (xf.c * xf.c + xf.d * xf.d).sqrt();
                let scale = ((sx + sy) * 0.5).max(0.01);
                let fs = b.style.font_size * scale;
                let ta = scale_alpha(255, opacity);
                let color = Color {
                    r: b.style.color.0,
                    g: b.style.color.1,
                    b: b.style.color.2,
                    a: ta,
                };
                let baseline = dy + fs * 0.8;
                draw_run(
                    fb,
                    font,
                    s,
                    dx,
                    baseline,
                    fs,
                    color,
                    b.style.bold,
                    b.style.letter_spacing * scale,
                );
            }
        }

        // (c2) Caret: the focused-field text cursor. A solid thin bar filling the content rect in
        // the foreground color (mapped through the affine like any other box).
        if matches!(b.content, layout::BoxContent::Caret) {
            let ca = scale_alpha(255, opacity);
            let cc = Color {
                r: b.style.color.0,
                g: b.style.color.1,
                b: b.style.color.2,
                a: ca,
            };
            fill_box(
                fb,
                xf,
                content.x,
                content.y,
                content.width,
                content.height,
                0.0,
                cc,
                axis,
            );
        }

        // (c3) Form widget: a checkbox/radio, range slider, color swatch, or progress/meter bar,
        // drawn as primitive shapes (no glyphs) in the content rect.
        if let layout::BoxContent::Widget(kind) = &b.content {
            paint_widget(fb, xf, content, *kind, opacity);
        }

        // (d) Replaced image content: blit the decoded pixels into the content rect, scaled.
        // (Axis-aligned transforms map the destination rect exactly; rotation is approximated by
        // the bounding box.)
        if let layout::BoxContent::Image(node) = &b.content {
            let dst = xf_rect(xf, content.x, content.y, content.width, content.height);
            if dst.y < clip_bottom as i32 {
                // A <canvas> resolves to its rasterized display-list bitmap, an inline <svg> to its
                // rasterized subtree bitmap, everything else to a decoded <img>. All composite
                // identically.
                match canvas_bitmaps
                    .get(node)
                    .or_else(|| svg_bitmaps.get(node))
                    .or_else(|| images.get(node))
                {
                    Some(img) if opacity >= 0.999 => fb.blit_rgba(dst, &img.rgba, img.w, img.h),
                    Some(img) => {
                        let mut scaled = img.rgba.clone();
                        for px in scaled.chunks_exact_mut(4) {
                            px[3] = scale_alpha(px[3], opacity);
                        }
                        fb.blit_rgba(dst, &scaled, img.w, img.h);
                    }
                    None => {
                        let ph = Color {
                            r: 140,
                            g: 140,
                            b: 150,
                            a: scale_alpha(120, opacity),
                        };
                        if dst.w > 0 && dst.h > 0 {
                            fb.fill_rect(
                                Rect {
                                    x: dst.x,
                                    y: dst.y,
                                    w: dst.w,
                                    h: 1,
                                },
                                ph,
                            );
                            fb.fill_rect(
                                Rect {
                                    x: dst.x,
                                    y: dst.y + dst.h - 1,
                                    w: dst.w,
                                    h: 1,
                                },
                                ph,
                            );
                            fb.fill_rect(
                                Rect {
                                    x: dst.x,
                                    y: dst.y,
                                    w: 1,
                                    h: dst.h,
                                },
                                ph,
                            );
                            fb.fill_rect(
                                Rect {
                                    x: dst.x + dst.w - 1,
                                    y: dst.y,
                                    w: 1,
                                    h: dst.h,
                                },
                                ph,
                            );
                        }
                    }
                }
            }
        }
    }

    // Advance the text-run counter for every non-empty Text run, INDEPENDENT of offscreen culling /
    // opacity, so it stays in lockstep with `collect_text_runs`' DFS order (which the selection
    // ranges were built against).
    if let layout::BoxContent::Text(s) = &b.content {
        if !s.is_empty() {
            *run_idx += 1;
        }
    }

    for child in &b.children {
        paint_box_opacity(
            fb,
            font,
            child,
            xf,
            clip_top,
            clip_bottom,
            images,
            canvas_bitmaps,
            svg_bitmaps,
            mask_bitmaps,
            opacity,
            sel_ranges,
            run_idx,
        );
    }
}

/// Fill a CSS-space rect through an affine: axis-aligned → a (rounded) device rect; otherwise a
/// transformed quad (rounding ignored). `radius` only applies in the axis-aligned case.
pub(crate) fn fill_box(
    fb: &mut Framebuffer,
    xf: &Affine,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    radius: f32,
    c: Color,
    axis: bool,
) {
    if axis {
        fb.fill_round_rect(xf_rect(xf, x, y, w, h), radius, c);
    } else {
        let p0 = xf.apply(x, y);
        let p1 = xf.apply(x + w, y);
        let p2 = xf.apply(x + w, y + h);
        let p3 = xf.apply(x, y + h);
        fill_quad(fb, [p0, p1, p2, p3], c);
    }
}

/// Stroke a rectangle outline `t` device-px thick (inside the rect's edges), source-over at `c`.
/// Used for widget borders (checkbox/swatch/bar tracks). Coordinates are device px.
pub(crate) fn stroke_rect(fb: &mut Framebuffer, r: Rect, t: i32, c: Color) {
    if r.w <= 0 || r.h <= 0 || t <= 0 {
        return;
    }
    let t = t.min(r.w).min(r.h);
    fb.fill_rect(
        Rect {
            x: r.x,
            y: r.y,
            w: r.w,
            h: t,
        },
        c,
    ); // top
    fb.fill_rect(
        Rect {
            x: r.x,
            y: r.y + r.h - t,
            w: r.w,
            h: t,
        },
        c,
    ); // bottom
    fb.fill_rect(
        Rect {
            x: r.x,
            y: r.y,
            w: t,
            h: r.h,
        },
        c,
    ); // left
    fb.fill_rect(
        Rect {
            x: r.x + r.w - t,
            y: r.y,
            w: t,
            h: r.h,
        },
        c,
    ); // right
}

/// Fill a circle (device-space center `cx,cy`, radius `rad`) source-over at `c`, with 1px AA at the
/// rim. Used for radio dials and range thumbs.
pub(crate) fn fill_circle(fb: &mut Framebuffer, cx: f32, cy: f32, rad: f32, c: Color) {
    if rad <= 0.0 {
        return;
    }
    let x0 = (cx - rad).floor() as i32;
    let x1 = (cx + rad).ceil() as i32;
    let y0 = (cy - rad).floor() as i32;
    let y1 = (cy + rad).ceil() as i32;
    for y in y0..=y1 {
        for x in x0..=x1 {
            let dx = x as f32 + 0.5 - cx;
            let dy = y as f32 + 0.5 - cy;
            let d = (dx * dx + dy * dy).sqrt();
            // Full coverage inside (rad-1), linear AA over the outer 1px band.
            let cov = (rad - d + 0.5).clamp(0.0, 1.0);
            if cov > 0.0 {
                // blend_coverage folds c.a in itself; pass coverage as 0..=255.
                fb.blend_coverage(x, y, (cov * 255.0).round() as u8, c);
            }
        }
    }
}

/// Draw a 1px-ish line between two device-space points (Bresenham, `t`-px square brush) at `c`.
/// Used for the checkbox check mark's two strokes.
pub(crate) fn draw_line(
    fb: &mut Framebuffer,
    x0: f32,
    y0: f32,
    x1: f32,
    y1: f32,
    t: i32,
    c: Color,
) {
    let t = t.max(1);
    let (mut x0, mut y0) = (x0.round() as i32, y0.round() as i32);
    let (x1, y1) = (x1.round() as i32, y1.round() as i32);
    let dx = (x1 - x0).abs();
    let dy = -(y1 - y0).abs();
    let sx = if x0 < x1 { 1 } else { -1 };
    let sy = if y0 < y1 { 1 } else { -1 };
    let mut err = dx + dy;
    loop {
        fb.fill_rect(
            Rect {
                x: x0 - t / 2,
                y: y0 - t / 2,
                w: t,
                h: t,
            },
            c,
        );
        if x0 == x1 && y0 == y1 {
            break;
        }
        let e2 = 2 * err;
        if e2 >= dy {
            err += dy;
            x0 += sx;
        }
        if e2 <= dx {
            err += dx;
            y0 += sy;
        }
    }
}

/// Paint a drawn form widget into its (CSS-space) content rect, mapped through the affine. Widgets
/// are primitive shapes only (no glyphs): a checkbox/radio, a range slider, a color swatch, or a
/// progress/meter bar. Sized at layout-build time; here we just rasterize.
pub(crate) fn paint_widget(
    fb: &mut Framebuffer,
    xf: &Affine,
    content: layout::Rect,
    kind: layout::WidgetKind,
    opacity: f32,
) {
    let a = scale_alpha(255, opacity);
    // Device-space content rect.
    let r = xf_rect(xf, content.x, content.y, content.width, content.height);
    if r.w <= 0 || r.h <= 0 {
        return;
    }
    let border = Color {
        r: 118,
        g: 118,
        b: 118,
        a,
    }; // #767676, the UA control border
    match kind {
        layout::WidgetKind::Checkbox { checked } => {
            // A square box (centered in the content rect), white fill, gray border; a check mark
            // (two strokes) when checked. The font lacks ☑/☐ so we draw it.
            let s = r.w.min(r.h);
            let bx = r.x + (r.w - s) / 2;
            let by = r.y + (r.h - s) / 2;
            let sq = Rect {
                x: bx,
                y: by,
                w: s,
                h: s,
            };
            fb.fill_round_rect(
                sq,
                2.0,
                Color {
                    r: 255,
                    g: 255,
                    b: 255,
                    a,
                },
            );
            stroke_rect(fb, sq, 1, border);
            if checked {
                // A blue-ish fill behind a white check, or just a dark check on white. We draw a
                // filled accent box + white check for a clear "on" state.
                let inset = (s as f32 * 0.0).max(0.0) as i32;
                let acc = Rect {
                    x: bx + inset,
                    y: by + inset,
                    w: s - 2 * inset,
                    h: s - 2 * inset,
                };
                fb.fill_round_rect(
                    acc,
                    2.0,
                    Color {
                        r: 36,
                        g: 110,
                        b: 230,
                        a,
                    },
                ); // accent blue
                let fx = bx as f32;
                let fy = by as f32;
                let fs = s as f32;
                let check = Color {
                    r: 255,
                    g: 255,
                    b: 255,
                    a,
                };
                let t = (fs / 7.0).round().clamp(1.0, 3.0) as i32;
                // ✓ : down-stroke from (0.22,0.52) to (0.42,0.74), up-stroke to (0.78,0.28).
                draw_line(
                    fb,
                    fx + fs * 0.24,
                    fy + fs * 0.52,
                    fx + fs * 0.43,
                    fy + fs * 0.72,
                    t,
                    check,
                );
                draw_line(
                    fb,
                    fx + fs * 0.43,
                    fy + fs * 0.72,
                    fx + fs * 0.76,
                    fy + fs * 0.30,
                    t,
                    check,
                );
            }
        }
        layout::WidgetKind::Radio { checked } => {
            // A circle outline; a filled inner dot when checked.
            let s = r.w.min(r.h);
            let cx = r.x as f32 + r.w as f32 / 2.0;
            let cy = r.y as f32 + r.h as f32 / 2.0;
            let rad = s as f32 / 2.0;
            fill_circle(
                fb,
                cx,
                cy,
                rad,
                Color {
                    r: 255,
                    g: 255,
                    b: 255,
                    a,
                },
            );
            // Border ring: a slightly larger circle in border color, then the white re-fill.
            fill_circle(fb, cx, cy, rad, border);
            fill_circle(
                fb,
                cx,
                cy,
                (rad - 1.0).max(0.5),
                Color {
                    r: 255,
                    g: 255,
                    b: 255,
                    a,
                },
            );
            if checked {
                fill_circle(
                    fb,
                    cx,
                    cy,
                    (rad * 0.5).max(1.0),
                    Color {
                        r: 36,
                        g: 110,
                        b: 230,
                        a,
                    },
                );
            }
        }
        layout::WidgetKind::Range { fraction } => {
            // A thin rounded track centered vertically, with a circular thumb at `fraction`.
            let track_h = (r.h / 4).max(3);
            let ty = r.y + (r.h - track_h) / 2;
            let pad = r.h / 2; // keep the thumb inside the box at the extremes
            let track = Rect {
                x: r.x + pad,
                y: ty,
                w: (r.w - 2 * pad).max(1),
                h: track_h,
            };
            fb.fill_round_rect(
                track,
                track_h as f32 / 2.0,
                Color {
                    r: 180,
                    g: 180,
                    b: 180,
                    a,
                },
            );
            // Filled (left) portion in the accent color.
            let filled_w = (track.w as f32 * fraction.clamp(0.0, 1.0)).round() as i32;
            if filled_w > 0 {
                fb.fill_round_rect(
                    Rect {
                        x: track.x,
                        y: ty,
                        w: filled_w,
                        h: track_h,
                    },
                    track_h as f32 / 2.0,
                    Color {
                        r: 36,
                        g: 110,
                        b: 230,
                        a,
                    },
                );
            }
            let thumb_cx = track.x as f32 + track.w as f32 * fraction.clamp(0.0, 1.0);
            let thumb_cy = r.y as f32 + r.h as f32 / 2.0;
            let thumb_r = (r.h as f32 / 2.0 - 1.0).max(3.0);
            fill_circle(
                fb,
                thumb_cx,
                thumb_cy,
                thumb_r,
                Color {
                    r: 240,
                    g: 240,
                    b: 240,
                    a,
                },
            );
            fill_circle(fb, thumb_cx, thumb_cy, thumb_r, border);
            fill_circle(
                fb,
                thumb_cx,
                thumb_cy,
                (thumb_r - 1.0).max(1.0),
                Color {
                    r: 245,
                    g: 245,
                    b: 245,
                    a,
                },
            );
        }
        layout::WidgetKind::Color { rgb } => {
            // A swatch filled with the chosen color, thin gray border (with a small inner inset so
            // the color reads clearly).
            stroke_rect(fb, r, 1, border);
            let inner = Rect {
                x: r.x + 2,
                y: r.y + 2,
                w: (r.w - 4).max(1),
                h: (r.h - 4).max(1),
            };
            fb.fill_rect(
                inner,
                Color {
                    r: rgb.0,
                    g: rgb.1,
                    b: rgb.2,
                    a,
                },
            );
        }
        layout::WidgetKind::Progress { fraction } => {
            // A rounded track (light bg) with a blue filled portion = fraction. Indeterminate
            // (None) → a fully filled track (a reasonable static stand-in for the animation).
            let rad = (r.h as f32 / 2.0).min(6.0);
            fb.fill_round_rect(
                r,
                rad,
                Color {
                    r: 225,
                    g: 225,
                    b: 225,
                    a,
                },
            );
            stroke_rect(
                fb,
                r,
                1,
                Color {
                    r: 190,
                    g: 190,
                    b: 190,
                    a,
                },
            );
            let frac = fraction.unwrap_or(1.0).clamp(0.0, 1.0);
            let fw = (r.w as f32 * frac).round() as i32;
            if fw > 0 {
                fb.fill_round_rect(
                    Rect {
                        x: r.x,
                        y: r.y,
                        w: fw,
                        h: r.h,
                    },
                    rad,
                    Color {
                        r: 36,
                        g: 110,
                        b: 230,
                        a,
                    },
                );
            }
        }
        layout::WidgetKind::Meter { fraction } => {
            // Like progress but a greenish fill.
            let rad = (r.h as f32 / 2.0).min(6.0);
            fb.fill_round_rect(
                r,
                rad,
                Color {
                    r: 225,
                    g: 225,
                    b: 225,
                    a,
                },
            );
            stroke_rect(
                fb,
                r,
                1,
                Color {
                    r: 190,
                    g: 190,
                    b: 190,
                    a,
                },
            );
            let fw = (r.w as f32 * fraction.clamp(0.0, 1.0)).round() as i32;
            if fw > 0 {
                fb.fill_round_rect(
                    Rect {
                        x: r.x,
                        y: r.y,
                        w: fw,
                        h: r.h,
                    },
                    rad,
                    Color {
                        r: 76,
                        g: 174,
                        b: 80,
                        a,
                    },
                );
            }
        }
    }
}

/// Rasterize a (convex) quadrilateral given its 4 device-space corners (in order), source-over at
/// `c`. Used to paint rotated/skewed backgrounds and borders. Scanline fill over the bounding box,
/// testing each pixel center for inclusion (consistent winding). Used only off the no-transform
/// fast path.
pub(crate) fn fill_quad(fb: &mut Framebuffer, pts: [(f32, f32); 4], c: Color) {
    let xs = [pts[0].0, pts[1].0, pts[2].0, pts[3].0];
    let ys = [pts[0].1, pts[1].1, pts[2].1, pts[3].1];
    let minx = xs.iter().cloned().fold(f32::MAX, f32::min).floor().max(0.0) as i32;
    let maxx = xs
        .iter()
        .cloned()
        .fold(f32::MIN, f32::max)
        .ceil()
        .min(fb.width as f32) as i32;
    let miny = ys.iter().cloned().fold(f32::MAX, f32::min).floor().max(0.0) as i32;
    let maxy = ys
        .iter()
        .cloned()
        .fold(f32::MIN, f32::max)
        .ceil()
        .min(fb.height as f32) as i32;
    if maxx <= minx || maxy <= miny {
        return;
    }
    // Sign of the cross product of each edge with the point; convex quad → all same sign inside.
    let inside = |px: f32, py: f32| -> bool {
        let mut sign = 0.0_f32;
        for i in 0..4 {
            let (ax, ay) = pts[i];
            let (bx, by) = pts[(i + 1) % 4];
            let cross = (bx - ax) * (py - ay) - (by - ay) * (px - ax);
            if cross.abs() > 1e-6 {
                if sign == 0.0 {
                    sign = cross.signum();
                } else if cross.signum() != sign {
                    return false;
                }
            }
        }
        true
    };
    for y in miny..maxy {
        let py = y as f32 + 0.5;
        for x in minx..maxx {
            let px = x as f32 + 0.5;
            if inside(px, py) {
                let i = (y as u32 * fb.stride) as usize + (x as usize) * 4;
                blend_pixel(&mut fb.pixels[i..i + 4], c);
            }
        }
    }
}

/// Source-over one color onto a device pixel (mirrors paint's internal blend, exposed for the
/// engine's quad/gradient/shadow rasterizers).
pub(crate) fn blend_pixel(dst: &mut [u8], src: Color) {
    if src.a == 0 {
        return;
    }
    if src.a == 255 {
        dst[0] = src.r;
        dst[1] = src.g;
        dst[2] = src.b;
        dst[3] = 255;
        return;
    }
    let sa = src.a as u32;
    let ia = 255 - sa;
    dst[0] = ((src.r as u32 * sa + dst[0] as u32 * ia) / 255) as u8;
    dst[1] = ((src.g as u32 * sa + dst[1] as u32 * ia) / 255) as u8;
    dst[2] = ((src.b as u32 * sa + dst[2] as u32 * ia) / 255) as u8;
    dst[3] = (sa + dst[3] as u32 * ia / 255).min(255) as u8;
}

/// Fill a box's border-box with a gradient. Each device pixel inside the (axis-aligned) destination
/// rect is mapped back to the box's local 0..1 space, its gradient parameter `t` computed (linear:
/// projection onto the angle vector; radial: normalized distance from center), and the surrounding
/// color stops lerped. Respects `border_radius` (corner clipping like the solid fill) and `opacity`.
/// Non-axis-aligned transforms fall back to the bounding-box rect (rotation of the gradient itself
/// is approximate).
pub(crate) fn paint_gradient_fill(
    fb: &mut Framebuffer,
    xf: &Affine,
    border: layout::Rect,
    radius: f32,
    grad: &style::Gradient,
    opacity: f32,
    _axis: bool,
) {
    let dst = xf_rect(xf, border.x, border.y, border.width, border.height);
    let x0 = dst.x.max(0);
    let y0 = dst.y.max(0);
    let x1 = (dst.x + dst.w).min(fb.width as i32);
    let y1 = (dst.y + dst.h).min(fb.height as i32);
    if x1 <= x0 || y1 <= y0 {
        return;
    }
    let dw = (dst.w.max(1)) as f32;
    let dh = (dst.h.max(1)) as f32;
    let r = radius
        .min(dst.w as f32 / 2.0)
        .min(dst.h as f32 / 2.0)
        .max(0.0);
    // Linear gradient axis direction in normalized box space (CSS angle: 0=up, 90=right).
    let (dirx, diry, half_len);
    match grad {
        style::Gradient::Linear { angle_deg, .. } => {
            let a = angle_deg.to_radians();
            // Direction the gradient progresses (toward 100%).
            dirx = a.sin();
            diry = -a.cos();
            // Projection length so the gradient spans corner-to-corner along the axis.
            half_len = (dw * dirx.abs() + dh * diry.abs()) * 0.5;
        }
        style::Gradient::Radial { .. } => {
            dirx = 0.0;
            diry = 0.0;
            half_len = (dw * dw + dh * dh).sqrt() * 0.5;
        }
    }
    let cx = (x0 + x1) as f32 * 0.5;
    let cy = (y0 + y1) as f32 * 0.5;
    let stops = match grad {
        style::Gradient::Linear { stops, .. } => stops,
        style::Gradient::Radial { stops } => stops,
    };
    for y in y0..y1 {
        let py = y as f32 + 0.5;
        let row = (y as u32 * fb.stride) as usize;
        for x in x0..x1 {
            let px = x as f32 + 0.5;
            // Rounded-corner clip (matches fill_round_rect).
            if r > 0.0 && !inside_round_rect(px, py, dst, r) {
                continue;
            }
            let t = match grad {
                style::Gradient::Linear { .. } => {
                    let proj = (px - cx) * dirx + (py - cy) * diry;
                    if half_len > 0.0 {
                        (proj / half_len) * 0.5 + 0.5
                    } else {
                        0.5
                    }
                }
                style::Gradient::Radial { .. } => {
                    let dist = ((px - cx).powi(2) + (py - cy).powi(2)).sqrt();
                    if half_len > 0.0 {
                        dist / half_len
                    } else {
                        0.0
                    }
                }
            };
            let col = sample_stops(stops, t.clamp(0.0, 1.0));
            let a = scale_alpha(col.a, opacity);
            if a == 0 {
                continue;
            }
            let i = row + (x as usize) * 4;
            blend_pixel(
                &mut fb.pixels[i..i + 4],
                Color {
                    r: col.r,
                    g: col.g,
                    b: col.b,
                    a,
                },
            );
        }
    }
}

/// True if a pixel center lies inside a rounded rect (used to clip the gradient/shadow corners).
pub(crate) fn inside_round_rect(px: f32, py: f32, rect: Rect, r: f32) -> bool {
    let left_cx = rect.x as f32 + r;
    let right_cx = (rect.x + rect.w) as f32 - r;
    let top_cy = rect.y as f32 + r;
    let bottom_cy = (rect.y + rect.h) as f32 - r;
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
    match (cx, cy) {
        (Some(cx), Some(cy)) => ((px - cx).powi(2) + (py - cy).powi(2)).sqrt() <= r,
        _ => true,
    }
}

/// Linearly interpolate the gradient stops at parameter `t` in 0..1 (stops sorted by `pos`).
pub(crate) fn sample_stops(stops: &[style::GradientStop], t: f32) -> style::Rgba {
    if stops.is_empty() {
        return style::Rgba {
            r: 0,
            g: 0,
            b: 0,
            a: 0,
        };
    }
    if t <= stops[0].pos {
        return stops[0].color;
    }
    let last = stops.len() - 1;
    if t >= stops[last].pos {
        return stops[last].color;
    }
    for i in 0..last {
        let a = stops[i];
        let b = stops[i + 1];
        if t >= a.pos && t <= b.pos {
            let span = (b.pos - a.pos).max(1e-6);
            let f = (t - a.pos) / span;
            return style::Rgba {
                r: lerp_u8(a.color.r, b.color.r, f),
                g: lerp_u8(a.color.g, b.color.g, f),
                b: lerp_u8(a.color.b, b.color.b, f),
                a: lerp_u8(a.color.a, b.color.a, f),
            };
        }
    }
    stops[last].color
}

pub(crate) fn lerp_u8(a: u8, b: u8, f: f32) -> u8 {
    (a as f32 + (b as f32 - a as f32) * f)
        .round()
        .clamp(0.0, 255.0) as u8
}

/// Paint one box-shadow layer. OUTER: a rect offset by (dx,dy), inflated by `spread`, with a `blur`
/// px feathered edge (concentric alpha-decreasing bands approximating a Gaussian falloff). INSET:
/// a feathered inner band along the box edges. Border-radius rounding is honored for the solid
/// core (corner clip) but the feather is rectangular. Honors `opacity`.
pub(crate) fn paint_box_shadow(
    fb: &mut Framebuffer,
    xf: &Affine,
    border: layout::Rect,
    radius: f32,
    sh: &style::BoxShadow,
    opacity: f32,
    inset: bool,
) {
    let base_a = scale_alpha(sh.color.a, opacity);
    if base_a == 0 {
        return;
    }
    let col = |a: u8| Color {
        r: sh.color.r,
        g: sh.color.g,
        b: sh.color.b,
        a,
    };
    if !inset {
        // Outer: core rect = border box, offset by (dx,dy), inflated by spread.
        let bx = border.x + sh.dx - sh.spread;
        let by = border.y + sh.dy - sh.spread;
        let bw = border.width + 2.0 * sh.spread;
        let bh = border.height + 2.0 * sh.spread;
        let core = xf_rect(xf, bx, by, bw, bh);
        let blur = sh.blur.max(0.0);
        if blur <= 0.5 {
            // Hard shadow: a single solid (rounded) rect.
            fb.fill_round_rect(core, radius, col(base_a));
            return;
        }
        // Feather: draw the core, then expand outward in 1px bands with linearly falling alpha.
        let bands = blur.ceil() as i32;
        fb.fill_round_rect(core, radius, col(base_a));
        for k in 1..=bands {
            let frac = 1.0 - (k as f32 / (bands as f32 + 1.0));
            let a = (base_a as f32 * frac * 0.6).round() as u8;
            if a == 0 {
                continue;
            }
            let ring = Rect {
                x: core.x - k,
                y: core.y - k,
                w: core.w + 2 * k,
                h: core.h + 2 * k,
            };
            // Draw just the 1px ring (top/bottom/left/right strips) to avoid re-darkening the core.
            fb.fill_rect(
                Rect {
                    x: ring.x,
                    y: ring.y,
                    w: ring.w,
                    h: 1,
                },
                col(a),
            );
            fb.fill_rect(
                Rect {
                    x: ring.x,
                    y: ring.y + ring.h - 1,
                    w: ring.w,
                    h: 1,
                },
                col(a),
            );
            fb.fill_rect(
                Rect {
                    x: ring.x,
                    y: ring.y,
                    w: 1,
                    h: ring.h,
                },
                col(a),
            );
            fb.fill_rect(
                Rect {
                    x: ring.x + ring.w - 1,
                    y: ring.y,
                    w: 1,
                    h: ring.h,
                },
                col(a),
            );
        }
    } else {
        // Inset (best-effort): a feathered band just inside the border box, offset by (dx,dy).
        let inner = xf_rect(xf, border.x, border.y, border.width, border.height);
        let blur = sh.blur.max(1.0);
        let bands = (blur + sh.spread.abs()).ceil().max(1.0) as i32;
        for k in 0..bands {
            let frac = 1.0 - (k as f32 / (bands as f32));
            let a = (base_a as f32 * frac * 0.5).round() as u8;
            if a == 0 {
                continue;
            }
            let dxk = sh.dx.round() as i32;
            let dyk = sh.dy.round() as i32;
            // Top & left bands shift with the offset.
            fb.fill_rect(
                Rect {
                    x: inner.x + dxk,
                    y: inner.y + k + dyk,
                    w: inner.w,
                    h: 1,
                },
                col(a),
            );
            fb.fill_rect(
                Rect {
                    x: inner.x + k + dxk,
                    y: inner.y + dyk,
                    w: 1,
                    h: inner.h,
                },
                col(a),
            );
            fb.fill_rect(
                Rect {
                    x: inner.x + dxk,
                    y: inner.y + inner.h - 1 - k + dyk,
                    w: inner.w,
                    h: 1,
                },
                col(a),
            );
            fb.fill_rect(
                Rect {
                    x: inner.x + inner.w - 1 - k + dxk,
                    y: inner.y + dyk,
                    w: 1,
                    h: inner.h,
                },
                col(a),
            );
        }
    }
}

/// A simple computed vertical gradient — proof the pixels came from our code.
/// The page canvas background. CSS "background propagation": the canvas takes the root element's
/// (`<html>`) background; if that's transparent, the `<body>`'s background propagates up. Defaults
/// to white when neither sets one. Walks the first-child chain (html → body) so it never picks up a
/// content element's background.
pub(crate) fn page_background(root: &layout::LayoutBox, root_scheme_dark: bool) -> Color {
    let mut node = root;
    for _ in 0..3 {
        if let Some((r, g, b)) = node.style.background_color {
            return Color::rgb(r, g, b);
        }
        match node.children.first() {
            Some(c) => node = c,
            None => break,
        }
    }
    // No explicit html/body background: default canvas is white, or dark (`#1e1e1e`) when the page
    // opted into a dark `color-scheme` (resolved during the cascade and stored on the layout cache).
    if root_scheme_dark {
        Color::rgb(0x1e, 0x1e, 0x1e)
    } else {
        Color::WHITE
    }
}

/// Draw a macOS-style overlay scrollbar (a semi-transparent rounded thumb on the right edge) when
/// the document is taller than the viewport. `top` is the content area's top (device px), `viewport_h`
/// its height, `content_h` the full document height, `scroll_y` the current offset — all device px.
pub(crate) fn paint_scrollbar(
    fb: &mut Framebuffer,
    top: f32,
    viewport_h: f32,
    content_h: f32,
    scroll_y: f32,
    scale: f32,
) {
    if content_h <= viewport_h + 1.0 || viewport_h <= 1.0 {
        return; // nothing to scroll
    }
    let w = (7.0 * scale).round().max(4.0);
    let margin = 2.0 * scale;
    let x = (fb.width as f32 - w - margin).round() as i32;
    let min_thumb = 28.0 * scale;
    let thumb_h = ((viewport_h / content_h) * viewport_h).clamp(min_thumb, viewport_h);
    let max_off = (viewport_h - thumb_h).max(0.0);
    let frac = (scroll_y / (content_h - viewport_h)).clamp(0.0, 1.0);
    let y = (top + frac * max_off).round() as i32;
    // Semi-transparent neutral grey reads on both light and dark pages.
    let thumb = Color {
        r: 128,
        g: 128,
        b: 128,
        a: 150,
    };
    fb.fill_round_rect(
        Rect {
            x,
            y,
            w: w.round() as i32,
            h: thumb_h.round() as i32,
        },
        w / 2.0,
        thumb,
    );
}

pub(crate) fn paint_gradient(fb: &mut Framebuffer) {
    let h = fb.height.max(1);
    for y in 0..fb.height {
        let t = y as f32 / h as f32;
        let c = Color::rgb(
            (18.0 + t * 10.0) as u8,
            (20.0 + t * 14.0) as u8,
            (28.0 + t * 26.0) as u8,
        );
        fb.fill_rect(
            Rect {
                x: 0,
                y: y as i32,
                w: fb.width as i32,
                h: 1,
            },
            c,
        );
    }
}

/// Draw a left-anchored string with its baseline at `baseline_y`. Returns the final pen x.
pub(crate) fn draw_text(
    fb: &mut Framebuffer,
    font: &dyn GlyphRasterizer,
    text: &str,
    x: f32,
    baseline_y: f32,
    px: f32,
    color: Color,
) -> f32 {
    draw_text_spaced(fb, font, text, x, baseline_y, px, color, 0.0)
}

/// Like [`draw_text`] but adds `letter_spacing` px to the pen after each character. Returns the
/// final pen x (after the last glyph's advance + spacing), used to size text-decoration lines.
#[allow(clippy::too_many_arguments)]
pub(crate) fn draw_text_spaced(
    fb: &mut Framebuffer,
    font: &dyn GlyphRasterizer,
    text: &str,
    x: f32,
    baseline_y: f32,
    px: f32,
    color: Color,
    letter_spacing: f32,
) -> f32 {
    let mut pen = x;
    for ch in text.chars() {
        if let Some(g) = font.rasterize(ch, px) {
            for row in 0..g.height {
                for col in 0..g.width {
                    let cov = g.coverage[row * g.width + col];
                    if cov == 0 {
                        continue;
                    }
                    let dx = pen as i32 + g.left + col as i32;
                    let dy = baseline_y as i32 + g.top + row as i32;
                    fb.blend_coverage(dx, dy, cov, color);
                }
            }
            pen += g.advance;
        } else {
            pen += font.advance(ch, px);
        }
        pen += letter_spacing;
    }
    pen
}
