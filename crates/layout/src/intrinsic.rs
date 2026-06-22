use crate::*;
use std::collections::HashMap;

// ---------------------------------------------------------------------------------------------
// Intrinsic sizing
// ---------------------------------------------------------------------------------------------

/// Estimate the intrinsic content width of a box: explicit width if set, else the widest line
/// of text it would produce laid out unconstrained (max-content), plus its descendants' needs.
/// Used by inline-block (to size atomically) and flex (content base size).
pub(crate) fn intrinsic_width(
    boxx: &LayoutBox,
    styles: &HashMap<dom::NodeId, style::ComputedStyle>,
    measurer: &dyn TextMeasurer,
) -> f32 {
    if let Some(w) = explicit_width(boxx, styles) {
        let p = boxx.dimensions.padding;
        let b = boxx.dimensions.border;
        return w + p.left + p.right + b.left + b.right;
    }
    // Sum of own horizontal padding/border, plus the max child requirement.
    let p = boxx.dimensions.padding;
    let b = boxx.dimensions.border;
    let edges = p.left + p.right + b.left + b.right;

    // Gather all words in the subtree; the intrinsic (max-content) inline width is the sum of
    // word widths on a single unwrapped line for the longest text run. We approximate with the
    // widest single contiguous run of text.
    let mut max_inline = 0.0f32;
    let mut words: Vec<InlineWord> = Vec::new();
    collect_inline_words(&boxx.children, &mut words);
    if !words.is_empty() {
        let mut line_w = 0.0f32;
        for (i, w) in words.iter().enumerate() {
            let fam = w.style.font_family.as_deref();
            let ww = run_width(
                measurer,
                &w.text,
                w.style.font_size,
                w.style.bold,
                w.style.letter_spacing,
                fam,
            );
            let sp = if i == 0 {
                0.0
            } else {
                measurer.text_width(" ", w.style.font_size, w.style.bold, fam)
            };
            line_w += ww + sp;
        }
        max_inline = line_w;
    }

    // Reserve room for a focused-field caret bar (an inline atomic) so a shrink-to-fit control
    // doesn't clip the caret that sits right after its value text.
    for c in &boxx.children {
        if matches!(c.content, BoxContent::Caret) {
            max_inline += c.dimensions.margin_box().width;
        }
    }

    // Block children: the box is at least as wide as its widest block child.
    let mut max_block = 0.0f32;
    for c in &boxx.children {
        if matches!(c.content, BoxContent::Block | BoxContent::Anonymous) {
            max_block = max_block.max(intrinsic_width(c, styles, measurer));
        }
    }

    edges + max_inline.max(max_block)
}
