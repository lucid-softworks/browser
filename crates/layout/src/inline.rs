use crate::*;
use std::collections::HashMap;

// ---------------------------------------------------------------------------------------------
// Inline layout (line boxes + text wrapping)
// ---------------------------------------------------------------------------------------------

/// Whether `c` is a whitespace character at which a line may break (so word-splitting separates on
/// it). Unicode whitespace qualifies EXCEPT the non-breaking spaces — `&nbsp;` (U+00A0), narrow
/// NBSP (U+202F), figure space (U+2007), and ZWNBSP/BOM (U+FEFF) — which stay glued into their word
/// and contribute width, per CSS white-space processing.
fn is_breaking_space(c: char) -> bool {
    c.is_whitespace() && !matches!(c, '\u{00A0}' | '\u{202F}' | '\u{2007}' | '\u{FEFF}')
}

/// Lay out the inline/text children of `boxx` into line boxes, replacing `boxx.children` with a
/// flat list of positioned `Text` boxes (one per wrapped line per run) plus any atomic
/// inline-block boxes positioned on their line. Returns total height. `align` is the text
/// alignment of the block establishing this inline context.
pub(crate) fn layout_inline_children(
    boxx: &mut LayoutBox,
    align: TextAlignLocal,
    text_indent: f32,
    ctx: Ctx,
    styles: &HashMap<dom::NodeId, style::ComputedStyle>,
    measurer: &dyn TextMeasurer,
) -> f32 {
    let content = boxx.dimensions.content;
    let avail = content.width.max(0.0);

    // Flatten inline content into a sequence of inline items: words and atomic inline-blocks.
    // We move children out so atomic boxes can be re-emitted into the new child list.
    let original_children = std::mem::take(&mut boxx.children);
    let mut items: Vec<InlineItem> = Vec::new();
    collect_inline_items(original_children, ctx, styles, measurer, &mut items);

    // Greedy line breaking. Each line is a list of (item, x-offset-within-line).
    struct PlacedLine {
        items: Vec<(InlineItem, f32)>, // item + x offset within line
        width: f32,
        height: f32,
        max_font_size: f32,
    }
    let mut lines: Vec<PlacedLine> = Vec::new();
    let mut cur: Vec<(InlineItem, f32)> = Vec::new();
    let mut cursor_x = 0.0f32;
    let mut max_fs = 0.0f32;
    let mut line_h = 0.0f32;

    for item in items {
        // A forced break (`<br>` / preserved `\n`) ends the current line unconditionally and starts
        // a fresh one — even when the line is empty (so consecutive breaks produce blank lines).
        if let InlineItem::Break { font_size } = &item {
            let h = if line_h > 0.0 {
                line_h
            } else {
                measurer.line_height(*font_size, None)
            };
            let fs = if max_fs > 0.0 { max_fs } else { *font_size };
            lines.push(PlacedLine {
                items: std::mem::take(&mut cur),
                width: cursor_x,
                height: h,
                max_font_size: fs,
            });
            cursor_x = 0.0;
            max_fs = 0.0;
            line_h = 0.0;
            continue;
        }
        let (w, fs, h, leads_space) = item.metrics(measurer);
        let space_w = if cur.is_empty() || !leads_space {
            0.0
        } else {
            measurer.text_width(" ", fs, false, item.family())
        };
        // `text-indent` narrows only the first line box (the one currently being filled while no
        // line has been emitted yet); later lines get the full available width.
        let line_avail = if lines.is_empty() {
            (avail - text_indent).max(0.0)
        } else {
            avail
        };
        if !cur.is_empty() && cursor_x + space_w + w > line_avail {
            lines.push(PlacedLine {
                items: std::mem::take(&mut cur),
                width: cursor_x,
                height: line_h,
                max_font_size: max_fs,
            });
            cursor_x = 0.0;
            max_fs = 0.0;
            line_h = 0.0;
        }
        if !cur.is_empty() {
            cursor_x += space_w;
        }
        max_fs = max_fs.max(fs);
        line_h = line_h.max(h);
        cur.push((item, cursor_x));
        cursor_x += w;
    }
    if !cur.is_empty() {
        lines.push(PlacedLine {
            items: cur,
            width: cursor_x,
            height: line_h,
            max_font_size: max_fs,
        });
    }

    // Emit positioned boxes per line.
    let mut new_children: Vec<LayoutBox> = Vec::new();
    let mut y = content.y;
    let mut total_h = 0.0f32;
    for (line_idx, line) in lines.iter().enumerate() {
        let line_font = if line.max_font_size > 0.0 {
            line.max_font_size
        } else {
            16.0
        };
        // The line advance is the tallest item's preferred line-height (its computed
        // `line-height` if set, else the font metric — both already folded into `line.height`).
        let lh = if line.height > 0.0 {
            line.height
        } else {
            measurer.line_height(line_font, None)
        };
        // The emitted Text box's own height matches the line advance.
        let text_lh = lh;
        // `text-indent` shifts the first line's start by the indent and shrinks its alignment box.
        let indent = if line_idx == 0 { text_indent } else { 0.0 };
        let line_avail = (avail - indent).max(0.0);
        let line_x = match align {
            TextAlignLocal::Left => content.x + indent,
            TextAlignLocal::Center => content.x + indent + (line_avail - line.width).max(0.0) / 2.0,
            TextAlignLocal::Right => content.x + indent + (line_avail - line.width).max(0.0),
        };

        // Words on this line: emit a `Text` box per contiguous run of words sharing the same
        // DOM node, so painted text can be traced back to its element (e.g. an `<a>`). Each run's
        // box is positioned at the run's starting x offset within the line (the offset of its
        // first word, which already accounts for inter-word/inter-run spacing). Visual output is
        // unchanged: the same words at the same positions, only split where the source node
        // changes. Atomic boxes are repositioned at their offset as before.
        //
        // A "run" accumulates the words' joined text, the run's start x offset (line-relative),
        // its node, the paint style of its first word, and the font size to measure at.
        struct Run {
            texts: Vec<String>,
            start_off: f32,
            node: Option<dom::NodeId>,
            style: PaintStyle,
        }
        let mut run: Option<Run> = None;
        let flush = |run: &mut Option<Run>, out: &mut Vec<LayoutBox>| {
            if let Some(r) = run.take() {
                let text = r.texts.join(" ");
                let ls = r.style.letter_spacing;
                // `vertical-align: sub|super` shifts the run off the line's baseline by ~0.3em
                // (of the run's own, already-reduced, font size). Super raises (smaller y), sub
                // lowers (larger y). Width/height are measured at the run's own font size so the
                // shifted sub/sup text keeps its reduced size.
                let run_fs = r.style.font_size;
                let voff = match r.style.vertical_align {
                    style::VerticalAlign::Super => -run_fs * 0.3,
                    style::VerticalAlign::Sub => run_fs * 0.3,
                    style::VerticalAlign::Baseline => 0.0,
                };
                let measure_fs = if run_fs > 0.0 { run_fs } else { line_font };
                let mut tb = LayoutBox::new(BoxContent::Text(text), r.style, r.node);
                let fam = tb.style.font_family.as_deref().map(|s| s.to_string());
                let w = run_width(
                    measurer,
                    &tb_text(&tb),
                    measure_fs,
                    false,
                    ls,
                    fam.as_deref(),
                );
                tb.dimensions.content = Rect {
                    x: line_x + r.start_off,
                    y: y + voff,
                    width: w,
                    height: text_lh,
                };
                out.push(tb);
            }
        };
        for (item, off) in &line.items {
            match item {
                InlineItem::Word {
                    text, style, node, ..
                } => {
                    match &mut run {
                        // Continue the current run only if the node matches.
                        Some(r) if r.node == *node => r.texts.push(text.clone()),
                        _ => {
                            flush(&mut run, &mut new_children);
                            run = Some(Run {
                                texts: vec![text.clone()],
                                start_off: *off,
                                node: *node,
                                style: style.clone(),
                            });
                        }
                    }
                }
                InlineItem::Break { .. } => {
                    // Breaks are consumed during line building and never placed onto a line.
                    flush(&mut run, &mut new_children);
                }
                InlineItem::Atomic(b) => {
                    // An atomic box interrupts any word run.
                    flush(&mut run, &mut new_children);
                    let mut ab = (**b).clone();
                    // Reposition so the atomic box's margin-box sits at (line_x + off, y).
                    let mb = ab.dimensions.margin_box();
                    let dx = (line_x + off) - mb.x;
                    let dy = y - mb.y;
                    shift_subtree(&mut ab, dx, dy);
                    new_children.push(ab);
                }
            }
        }
        flush(&mut run, &mut new_children);
        y += lh;
        total_h += lh;
    }

    boxx.children = new_children;
    let _ = ctx;
    total_h
}

/// Advance width of a text run including `letter-spacing` (added once per character).
pub(crate) fn run_width(
    measurer: &dyn TextMeasurer,
    text: &str,
    px: f32,
    bold: bool,
    letter_spacing: f32,
    family: Option<&str>,
) -> f32 {
    let base = measurer.text_width(text, px, bold, family);
    if letter_spacing != 0.0 {
        base + letter_spacing * text.chars().count() as f32
    } else {
        base
    }
}

/// Helper to read the text out of a Text box (for measuring).
pub(crate) fn tb_text(b: &LayoutBox) -> String {
    match &b.content {
        BoxContent::Text(t) => t.clone(),
        _ => String::new(),
    }
}

/// A single word with its paint style, ready for line breaking.
pub(crate) struct InlineWord {
    pub(crate) text: String,
    pub(crate) style: PaintStyle,
    /// The DOM node of the source text box this word came from. Carried for parity with
    /// `InlineItem::Word`; the intrinsic-sizing path that builds `InlineWord`s doesn't read it.
    #[allow(dead_code)]
    node: Option<dom::NodeId>,
}

/// An inline-level item participating in line layout: either a word or an atomic inline-block.
pub(crate) enum InlineItem {
    /// A word carrying the DOM node of its source text box (used for hit-testing). `leads_space` is
    /// whether an inter-word space precedes it on a line (true for normal words; false for a
    /// `white-space: pre` run, whose spaces are already inside `text`).
    Word {
        text: String,
        style: PaintStyle,
        node: Option<dom::NodeId>,
        leads_space: bool,
    },
    /// An atomic box (inline-block / inline-flex / inline-grid) already laid out at a tentative
    /// origin; it advances the pen by its margin-box width and is repositioned on its line.
    Atomic(Box<LayoutBox>),
    /// A forced line break (`<br>` or a preserved `\n`): ends the current line box. `font_size` lets
    /// an empty break line still advance by a sensible line height.
    Break { font_size: f32 },
}

impl InlineItem {
    /// The computed `font-family` of this item's text (for web-font selection), if it has any.
    fn family(&self) -> Option<&str> {
        match self {
            InlineItem::Word { style, .. } => style.font_family.as_deref(),
            _ => None,
        }
    }

    /// Returns (advance_width, font_size, height, leads_with_space). `height` is the item's
    /// preferred line advance: the element's computed `line-height` if set, else the font metric.
    fn metrics(&self, measurer: &dyn TextMeasurer) -> (f32, f32, f32, bool) {
        match self {
            InlineItem::Word {
                text,
                style,
                leads_space,
                ..
            } => {
                let w = run_width(
                    measurer,
                    text,
                    style.font_size,
                    style.bold,
                    style.letter_spacing,
                    style.font_family.as_deref(),
                );
                let lh = style.line_height.unwrap_or_else(|| {
                    measurer.line_height(style.font_size, style.font_family.as_deref())
                });
                (w, style.font_size, lh, *leads_space)
            }
            InlineItem::Atomic(b) => {
                let mb = b.dimensions.margin_box();
                (mb.width, b.style.font_size, mb.height, false)
            }
            InlineItem::Break { font_size } => (
                0.0,
                *font_size,
                measurer.line_height(*font_size, None),
                false,
            ),
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
pub(crate) enum TextAlignLocal {
    Left,
    Center,
    Right,
}

/// Recursively collect inline items from an inline subtree (consuming the boxes). Text boxes
/// contribute words; inline elements recurse; inline-block / inline-flex / inline-grid boxes
/// become atomic items (already laid out at a tentative origin by `make_atomic`).
pub(crate) fn collect_inline_items(
    children: Vec<LayoutBox>,
    ctx: Ctx,
    styles: &HashMap<dom::NodeId, style::ComputedStyle>,
    measurer: &dyn TextMeasurer,
    out: &mut Vec<InlineItem>,
) {
    for mut child in children {
        // An element whose display is inline-block (or inline-flex/grid) is atomic.
        let is_atomic = child.node.and_then(|n| styles.get(&n)).map(|cs| {
            matches!(
                cs.display,
                style::Display::InlineBlock
                    | style::Display::InlineFlex
                    | style::Display::InlineGrid
            )
        }) == Some(true);

        if is_atomic {
            // Lay the atomic box out as a block at a tentative origin (0,0). Its intrinsic width
            // (max content line, or explicit width) becomes its size; it'll be repositioned on
            // its line. We use the box's intrinsic width as the containing width so shrink-to-fit
            // applies.
            let m = child.dimensions.margin;
            let iw = intrinsic_width(&child, styles, measurer) + m.left + m.right;
            let containing = Rect {
                x: 0.0,
                y: 0.0,
                width: iw,
                height: 0.0,
            };
            layout_block(&mut child, containing, ctx, styles, measurer);
            out.push(InlineItem::Atomic(Box::new(child)));
            continue;
        }
        match &child.content {
            BoxContent::Text(text) => {
                // Carry the source text box's DOM node onto each word so emitted line `Text`
                // boxes can be traced back to their element for hit-testing.
                let node = child.node;
                if child.style.white_space.preserves_spaces() {
                    // `white-space: pre`/`pre-wrap`: the run is atomic — spaces are PRESERVED (no
                    // split) and no inter-word space precedes it (its spaces are inside `text`).
                    // Newlines were already split into separate runs + `LineBreak`s at build time.
                    if !text.is_empty() {
                        out.push(InlineItem::Word {
                            text: text.clone(),
                            style: child.style.clone(),
                            node,
                            leads_space: false,
                        });
                    }
                } else {
                    // Break the run into words at *breaking* spaces. This is Unicode whitespace
                    // EXCEPT the non-breaking spaces (`&nbsp;` U+00A0, narrow NBSP, figure space,
                    // ZWNBSP), which stay part of their word and contribute width — while other
                    // Unicode spaces (e.g. U+2001 EM QUAD) remain line-break opportunities. Empty
                    // splits (leading/trailing/consecutive separators) are dropped, like
                    // `split_whitespace`.
                    for word in text.split(is_breaking_space).filter(|w| !w.is_empty()) {
                        out.push(InlineItem::Word {
                            text: word.to_string(),
                            style: child.style.clone(),
                            node,
                            leads_space: true,
                        });
                    }
                }
            }
            BoxContent::LineBreak => {
                out.push(InlineItem::Break {
                    font_size: child.style.font_size,
                });
            }
            BoxContent::Inline => {
                collect_inline_items(child.children, ctx, styles, measurer, out);
            }
            BoxContent::Image(_) => {
                // An atomic inline image: position its (pre-sized) content box at a tentative
                // origin so its margin box is well-formed, then emit it as an atomic item. It
                // advances the line by its margin-box width and is repositioned on its line.
                let containing = Rect {
                    x: 0.0,
                    y: 0.0,
                    width: 0.0,
                    height: 0.0,
                };
                layout_image_box(&mut child, containing);
                out.push(InlineItem::Atomic(Box::new(child)));
            }
            BoxContent::Caret => {
                // The focused-field caret: an atomic, pre-sized thin bar. Like an image, give it a
                // well-formed margin box at a tentative origin, then flow it inline so it sits
                // right after the value text (its top margin centers it on the line).
                let containing = Rect {
                    x: 0.0,
                    y: 0.0,
                    width: 0.0,
                    height: 0.0,
                };
                layout_image_box(&mut child, containing);
                out.push(InlineItem::Atomic(Box::new(child)));
            }
            BoxContent::Widget(_) => {
                // A drawn form widget: pre-sized (content set at build time), replaced-like. Treat
                // it as an atomic inline box (like an image) so it advances the line by its
                // border-box width and is repositioned on its line.
                let containing = Rect {
                    x: 0.0,
                    y: 0.0,
                    width: 0.0,
                    height: 0.0,
                };
                layout_image_box(&mut child, containing);
                out.push(InlineItem::Atomic(Box::new(child)));
            }
            _ => {
                // Block-level content shouldn't appear in an inline context; ignore defensively.
            }
        }
    }
}

/// Recursively collect words from an inline subtree (used by intrinsic sizing). Inline element
/// boxes contribute their children's words.
pub(crate) fn collect_inline_words(children: &[LayoutBox], out: &mut Vec<InlineWord>) {
    for child in children {
        match &child.content {
            BoxContent::Text(text) => {
                for word in text.split_whitespace() {
                    out.push(InlineWord {
                        text: word.to_string(),
                        style: child.style.clone(),
                        node: child.node,
                    });
                }
            }
            BoxContent::Inline => {
                collect_inline_words(&child.children, out);
            }
            _ => {}
        }
    }
}
